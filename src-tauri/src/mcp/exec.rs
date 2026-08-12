//! Side-channel `session_exec` (PR-M3).
//!
//! Uses the same OpenSSH exec path as Tab complete — does **not** write the
//! interactive PTY. Aligns cwd via `CwdTracker` (best-effort).

use std::time::{Duration, Instant};

use serde::Serialize;

use super::config::McpConfig;
use crate::app_state::{SessionRuntime, SessionState};
use crate::cwd::shell_single_quote;
use crate::error::AppError;
use crate::ops_log;
use crate::ssh::openssh::{self, SideChannelExecResult};
use crate::ssh::transport::ConnectParams;

/// Hard upper bound for tool-supplied timeout.
pub const MAX_TIMEOUT_SECS: u64 = 120;
/// Soft concurrency limit per session.
pub const MAX_INFLIGHT_EXEC: u32 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct ExecToolResult {
    pub session_id: String,
    pub cwd_used: Option<String>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub warnings: Vec<String>,
}

/// Resolve session id from args or config default.
pub fn resolve_session_id(
    arg_sid: Option<&str>,
    cfg: &McpConfig,
) -> Result<String, ToolError> {
    if let Some(s) = arg_sid.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(s.to_string());
    }
    if let Some(s) = cfg
        .default_session_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Ok(s.to_string());
    }
    Err(ToolError::invalid(
        "session_id required (or set default_session_id in mcp.json)",
    ))
}

pub fn check_session_allowed(session_id: &str, cfg: &McpConfig) -> Result<(), ToolError> {
    if let Some(ref allow) = cfg.allowed_session_ids {
        if !allow.iter().any(|a| a == session_id) {
            return Err(ToolError::forbidden(
                "session_id not in allowed_session_ids",
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    /// Whether a client may safely retry the same operation.
    pub retryable: bool,
    /// Optional truncated diagnostic (stderr excerpt, no secrets).
    pub detail: Option<String>,
}

impl ToolError {
    fn base(code: &'static str, msg: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: msg.into(),
            retryable,
            detail: None,
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::base("session_not_found", msg, false)
    }
    pub fn not_connected(msg: impl Into<String>) -> Self {
        Self::base("not_connected", msg, true)
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::base("forbidden", msg, false)
    }
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::base("invalid_params", msg, false)
    }
    pub fn busy(msg: impl Into<String>) -> Self {
        Self::base("busy", msg, true)
    }
    pub fn timeout(msg: impl Into<String>) -> Self {
        Self::base("timeout", msg, true)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::base("internal", msg, false)
    }
    pub fn cancelled(msg: impl Into<String>) -> Self {
        Self::base("cancelled", msg, false)
    }
    pub fn file_not_found(msg: impl Into<String>) -> Self {
        Self::base("file_not_found", msg, false)
    }
    pub fn permission_denied(msg: impl Into<String>) -> Self {
        Self::base("permission_denied", msg, false)
    }
    pub fn disk_full(msg: impl Into<String>) -> Self {
        Self::base("disk_full", msg, false)
    }
    pub fn verify_failed(msg: impl Into<String>) -> Self {
        Self::base("verify_failed", msg, false)
    }
    pub fn transfer_failed(msg: impl Into<String>) -> Self {
        // Default non-retryable; callers mark network blips as retryable.
        Self::base("transfer_failed", msg, false)
    }
    pub fn sandbox_denied(msg: impl Into<String>) -> Self {
        Self::base("sandbox_denied", msg, false)
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let d = detail.into();
        if !d.is_empty() {
            self.detail = Some(truncate_detail(&d, 400));
        }
        self
    }

    pub fn mark_retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "error": self.code,
            "message": self.message,
            "retryable": self.retryable,
        });
        if let Some(ref d) = self.detail {
            v["detail"] = serde_json::Value::String(d.clone());
        }
        v
    }
}

fn truncate_detail(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.len() <= max {
        return t.to_string();
    }
    let mut end = max;
    while end > 0 && !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &t[..end])
}

impl From<AppError> for ToolError {
    fn from(e: AppError) -> Self {
        match e {
            AppError::NotConnected => Self::not_connected(e.to_string()),
            AppError::SessionNotFound(s) => Self::not_found(s),
            other => Self::internal(other.to_string()),
        }
    }
}

fn connect_params_from_rt(rt: &SessionRuntime) -> Result<ConnectParams, ToolError> {
    let cached = rt
        .cached
        .lock()
        .map_err(|_| ToolError::internal("cached lock poisoned"))?
        .clone()
        .ok_or_else(|| ToolError::not_connected("no credential cache for session"))?;
    let (cols, rows) = rt.term_size();
    Ok(ConnectParams {
        host: cached.host,
        port: cached.port,
        username: cached.username,
        auth: cached.auth,
        cols,
        rows,
    })
}

fn require_connected(rt: &SessionRuntime) -> Result<(), ToolError> {
    let state = rt
        .meta
        .lock()
        .map_err(|_| ToolError::internal("meta lock poisoned"))?
        .state
        .clone();
    if state != SessionState::Connected {
        return Err(ToolError::not_connected(format!(
            "session state is {state:?}, need Connected"
        )));
    }
    if !rt.pty_is_live() {
        return Err(ToolError::not_connected(
            "session marked Connected but PTY/transport is not live",
        ));
    }
    Ok(())
}

/// Build remote one-liner: optional `cd` then `bash -lc 'command'`.
pub fn build_remote_command(cwd: Option<&str>, command: &str) -> String {
    let cmd_q = shell_single_quote(command);
    match cwd {
        Some(c) if !c.is_empty() => {
            format!("cd -- {} && bash -lc {}", shell_single_quote(c), cmd_q)
        }
        _ => format!("bash -lc {cmd_q}"),
    }
}

fn truncate_str(s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    // Prefer char boundary.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[truncated]");
    (out, true)
}

/// Execute command on the session's host via side-channel SSH.
pub async fn session_exec(
    rt: &SessionRuntime,
    cfg: &McpConfig,
    command: &str,
    cwd_override: Option<&str>,
    timeout_secs: Option<u64>,
) -> Result<ExecToolResult, ToolError> {
    let command = command.trim();
    if command.is_empty() {
        return Err(ToolError::invalid("command must not be empty"));
    }
    require_connected(rt)?;
    check_session_allowed(&rt.id, cfg)?;

    // Concurrency gate.
    let inflight = rt
        .mcp_exec_inflight
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if inflight >= MAX_INFLIGHT_EXEC {
        rt.mcp_exec_inflight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        return Err(ToolError::busy(format!(
            "too many concurrent execs on this session (max {MAX_INFLIGHT_EXEC})"
        )));
    }

    let result = session_exec_inner(rt, cfg, command, cwd_override, timeout_secs).await;

    rt.mcp_exec_inflight
        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    result
}

async fn session_exec_inner(
    rt: &SessionRuntime,
    cfg: &McpConfig,
    command: &str,
    cwd_override: Option<&str>,
    timeout_secs: Option<u64>,
) -> Result<ExecToolResult, ToolError> {
    let mut warnings = Vec::new();

    let (cwd_used, cwd_for_cmd) = {
        let tracker = rt
            .cwd
            .lock()
            .map_err(|_| ToolError::internal("cwd lock poisoned"))?;
        if let Some(o) = cwd_override.map(str::trim).filter(|s| !s.is_empty()) {
            (Some(o.to_string()), Some(o.to_string()))
        } else if let Some(p) = tracker.last_known() {
            (Some(p.to_string()), Some(p.to_string()))
        } else {
            warnings.push("cwd_unknown: running without cd (remote $HOME)".into());
            (None, None)
        }
    };

    let timeout_secs = timeout_secs
        .unwrap_or(cfg.exec_timeout_secs)
        .clamp(1, MAX_TIMEOUT_SECS);
    let max_out = cfg.exec_max_output_bytes.max(1024);

    let params = connect_params_from_rt(rt)?;
    let remote = build_remote_command(cwd_for_cmd.as_deref(), command);
    let control_path = rt.control_path.lock().ok().and_then(|g| g.clone());

    ops_log::log(
        "MCP",
        &format!(
            "tool=session_exec sid={} timeout_s={} cmd_len={} cwd={}",
            &rt.id[..rt.id.len().min(8)],
            timeout_secs,
            command.len(),
            cwd_used.as_deref().unwrap_or("-")
        ),
    );

    let t0 = Instant::now();
    let exec_res: Result<SideChannelExecResult, AppError> = openssh::openssh_exec_raw(
        &params,
        &remote,
        &rt.side_channel_key,
        control_path.as_deref(),
        Duration::from_secs(timeout_secs),
    )
    .await;

    let duration_ms = t0.elapsed().as_millis() as u64;

    match exec_res {
        Ok(raw) => {
            let (stdout, t1) = truncate_str(raw.stdout, max_out);
            let (stderr, t2) = truncate_str(raw.stderr, max_out / 4);
            let truncated = t1 || t2 || raw.truncated;
            ops_log::log(
                "MCP",
                &format!(
                    "tool=session_exec sid={} ok exit={:?} ms={} out_len={} trunc={}",
                    &rt.id[..rt.id.len().min(8)],
                    raw.exit_code,
                    duration_ms,
                    stdout.len(),
                    truncated
                ),
            );
            Ok(ExecToolResult {
                session_id: rt.id.clone(),
                cwd_used,
                exit_code: raw.exit_code,
                stdout,
                stderr,
                truncated,
                duration_ms,
                timed_out: false,
                warnings,
            })
        }
        Err(AppError::Message(m)) if m.contains("timeout") || m.contains("超时") => {
            ops_log::log(
                "MCP",
                &format!(
                    "tool=session_exec sid={} err=timeout ms={}",
                    &rt.id[..rt.id.len().min(8)],
                    duration_ms
                ),
            );
            Err(ToolError::timeout(format!(
                "exec exceeded {timeout_secs}s"
            )))
        }
        Err(e) => {
            ops_log::log(
                "MCP",
                &format!(
                    "tool=session_exec sid={} err={} ms={}",
                    &rt.id[..rt.id.len().min(8)],
                    e,
                    duration_ms
                ),
            );
            Err(ToolError::from(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cmd_with_cwd_quotes() {
        let s = build_remote_command(Some("/tmp/a b"), "echo hi");
        assert!(s.contains("cd -- '/tmp/a b'"));
        assert!(s.contains("bash -lc 'echo hi'"));
    }

    #[test]
    fn build_cmd_quotes_inner_quote() {
        let s = build_remote_command(None, "echo 'x'");
        // shell_single_quote embeds '\'' for single quotes
        assert!(s.starts_with("bash -lc "));
        assert!(s.contains("echo"));
    }

    #[test]
    fn truncate_marks() {
        let (s, t) = truncate_str("abcdef".into(), 3);
        assert!(t);
        assert!(s.contains("truncated"));
    }
}
