//! MCP file upload / download (side-channel scp / ssh stdin-stdout).
//!
//! ## Modes
//! - **content**: small inline payloads (≤4 MiB). Always synchronous.
//! - **local_path** (scp): disk↔disk for large files (hundreds of MB+).
//!   Default **async** (returns `job_id`); poll with `session_file_transfer_status`.
//!
//! ## Safety (path + content uploads)
//! - **Atomic write**: stage to `*.anchorterm-part-<id>` then `mv` / rename.
//! - **Verify**: default size check; optional sha256.
//! - **Cleanup**: remove staging on fail/cancel (default on).
//!
//! Does **not** use the interactive PTY.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::config::McpConfig;
use super::exec::{check_session_allowed, ToolError, MAX_TIMEOUT_SECS};
use crate::app_state::{SessionRuntime, SessionState};
use crate::cwd::shell_single_quote;
use crate::error::AppError;
use crate::ops_log;
use crate::ssh::openssh;
use crate::ssh::transport::ConnectParams;

// ── limits ──────────────────────────────────────────────────────────────────

pub const DEFAULT_MAX_TRANSFER_BYTES: usize = 1_048_576;
pub const HARD_MAX_TRANSFER_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_CONTENT_TIMEOUT_SECS: u64 = 60;
const MAX_INFLIGHT: u32 = 2;
const MAX_JOBS: usize = 64;
const JOB_TTL_SECS: u64 = 3600;
pub const LARGE_FILE_HINT_BYTES: u64 = 16 * 1024 * 1024;

// ── verify ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    None,
    Size,
    Sha256,
}

impl VerifyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyMode::None => "none",
            VerifyMode::Size => "size",
            VerifyMode::Sha256 => "sha256",
        }
    }
}

pub fn parse_verify_mode(s: Option<&str>) -> Result<VerifyMode, ToolError> {
    match s.map(str::trim).filter(|x| !x.is_empty()) {
        None => Ok(VerifyMode::Size),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "none" | "off" | "false" | "0" => Ok(VerifyMode::None),
            "size" | "bytes" | "len" => Ok(VerifyMode::Size),
            "sha256" | "hash" | "sha" => Ok(VerifyMode::Sha256),
            other => Err(ToolError::invalid(format!(
                "verify must be none|size|sha256, got {other}"
            ))),
        },
    }
}

// ── results ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct UploadResult {
    pub session_id: String,
    pub remote_path: String,
    pub bytes: u64,
    pub mode: String,
    pub duration_ms: u64,
    pub warnings: Vec<String>,
    pub verified: bool,
    pub verify_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_transfer: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadResult {
    pub session_id: String,
    pub remote_path: String,
    pub bytes: u64,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    pub truncated: bool,
    pub duration_ms: u64,
    pub warnings: Vec<String>,
    pub verified: bool,
    pub verify_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_transfer: Option<bool>,
}

#[derive(Debug, Clone)]
struct TransferComplete {
    bytes: u64,
    verified: bool,
    verify_mode: VerifyMode,
    sha256: Option<String>,
    warnings: Vec<String>,
}

// ── async jobs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferJobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferJobSnapshot {
    pub job_id: String,
    pub session_id: String,
    pub direction: TransferDirection,
    pub local_path: String,
    pub remote_path: String,
    pub status: TransferJobStatus,
    /// High-level step: queued | preparing | transferring | verifying | finalizing | done | failed | cancelled
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub bytes_total: Option<u64>,
    /// Bytes observed on staging path so far (or final size when done).
    pub bytes_transferred: Option<u64>,
    /// Alias of bytes_transferred for Agent-friendly naming.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_done: Option<u64>,
    /// 0.0–100.0 when total is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    /// Rough throughput from last progress samples (bytes/s).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_per_sec: Option<u64>,
    /// Last progress sample time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_unix_ms: Option<u64>,
    /// How progress is measured (e.g. remote_stage_stat / local_stage_stat).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_source: Option<String>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
    pub verified: Option<bool>,
    pub verify_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleaned_up: Option<bool>,
    pub created_unix_ms: u64,
    pub started_unix_ms: Option<u64>,
    pub finished_unix_ms: Option<u64>,
    pub duration_ms: Option<u64>,
}

struct TransferJob {
    id: String,
    session_id: String,
    direction: TransferDirection,
    local_path: String,
    remote_path: String,
    cancel: Arc<AtomicBool>,
    state: Mutex<TransferJobState>,
}

struct TransferJobState {
    status: TransferJobStatus,
    phase: Option<String>,
    bytes_total: Option<u64>,
    bytes_transferred: Option<u64>,
    percent: Option<f64>,
    bytes_per_sec: Option<u64>,
    updated_unix_ms: Option<u64>,
    progress_source: Option<String>,
    error: Option<String>,
    warnings: Vec<String>,
    verified: Option<bool>,
    verify_mode: Option<String>,
    sha256: Option<String>,
    cleaned_up: Option<bool>,
    created_unix_ms: u64,
    started_unix_ms: Option<u64>,
    finished_unix_ms: Option<u64>,
}

impl TransferJob {
    fn snapshot(&self) -> TransferJobSnapshot {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let duration_ms = match (st.started_unix_ms, st.finished_unix_ms) {
            (Some(s), Some(f)) => Some(f.saturating_sub(s)),
            (Some(s), None) => Some(now_unix_ms().saturating_sub(s)),
            _ => None,
        };
        TransferJobSnapshot {
            job_id: self.id.clone(),
            session_id: self.session_id.clone(),
            direction: self.direction,
            local_path: self.local_path.clone(),
            remote_path: self.remote_path.clone(),
            status: st.status,
            phase: st.phase.clone(),
            bytes_total: st.bytes_total,
            bytes_transferred: st.bytes_transferred,
            bytes_done: st.bytes_transferred,
            percent: st.percent,
            bytes_per_sec: st.bytes_per_sec,
            updated_unix_ms: st.updated_unix_ms,
            progress_source: st.progress_source.clone(),
            error: st.error.clone(),
            warnings: st.warnings.clone(),
            verified: st.verified,
            verify_mode: st.verify_mode.clone(),
            sha256: st.sha256.clone(),
            cleaned_up: st.cleaned_up,
            created_unix_ms: st.created_unix_ms,
            started_unix_ms: st.started_unix_ms,
            finished_unix_ms: st.finished_unix_ms,
            duration_ms,
        }
    }
}

fn set_job_progress(
    job: &TransferJob,
    phase: &str,
    bytes_done: Option<u64>,
    bytes_total: Option<u64>,
    bytes_per_sec: Option<u64>,
    progress_source: Option<&str>,
) {
    let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
    st.phase = Some(phase.to_string());
    st.updated_unix_ms = Some(now_unix_ms());
    if let Some(d) = bytes_done {
        st.bytes_transferred = Some(d);
    }
    if let Some(t) = bytes_total {
        st.bytes_total = Some(t);
    }
    if let Some(bps) = bytes_per_sec {
        st.bytes_per_sec = Some(bps);
    }
    if let Some(src) = progress_source {
        st.progress_source = Some(src.to_string());
    }
    if let (Some(d), Some(t)) = (st.bytes_transferred, st.bytes_total) {
        if t > 0 {
            let p = (d as f64) * 100.0 / (t as f64);
            st.percent = Some(if p > 100.0 { 100.0 } else { (p * 10.0).round() / 10.0 });
        }
    }
}

#[derive(Clone, Default)]
pub struct JobRegistry {
    inner: Arc<Mutex<HashMap<String, Arc<TransferJob>>>>,
}

impl JobRegistry {
    fn insert(&self, job: Arc<TransferJob>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(job.id.clone(), job);
        Self::gc_locked(&mut g);
    }

    fn get(&self, id: &str) -> Option<Arc<TransferJob>> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.get(id).cloned()
    }

    pub fn list(&self, session_id: Option<&str>) -> Vec<TransferJobSnapshot> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::gc_locked(&mut g);
        let mut out: Vec<_> = g
            .values()
            .filter(|j| session_id.map(|s| j.session_id == s).unwrap_or(true))
            .map(|j| j.snapshot())
            .collect();
        out.sort_by(|a, b| b.created_unix_ms.cmp(&a.created_unix_ms));
        out
    }

    /// Count queued/running jobs (for concurrency caps).
    pub fn count_active(&self) -> usize {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.values()
            .filter(|j| {
                let st = j.state.lock().unwrap_or_else(|e| e.into_inner());
                matches!(
                    st.status,
                    TransferJobStatus::Queued | TransferJobStatus::Running
                )
            })
            .count()
    }

    pub fn cancel(&self, id: &str) -> Result<TransferJobSnapshot, ToolError> {
        let job = self
            .get(id)
            .ok_or_else(|| ToolError::not_found(format!("transfer job not found: {id}")))?;
        let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
        match st.status {
            TransferJobStatus::Succeeded
            | TransferJobStatus::Failed
            | TransferJobStatus::Cancelled => {}
            TransferJobStatus::Queued | TransferJobStatus::Running => {
                job.cancel.store(true, Ordering::SeqCst);
                st.phase = Some("cancelling".into());
                st.updated_unix_ms = Some(now_unix_ms());
                if st.status == TransferJobStatus::Queued {
                    st.status = TransferJobStatus::Cancelled;
                    st.phase = Some("cancelled".into());
                    st.finished_unix_ms = Some(now_unix_ms());
                    st.error = Some("cancelled before start".into());
                    st.cleaned_up = Some(true);
                }
            }
        }
        drop(st);
        Ok(job.snapshot())
    }

    fn gc_locked(map: &mut HashMap<String, Arc<TransferJob>>) {
        let now = now_unix_ms();
        map.retain(|_, j| {
            let st = j.state.lock().unwrap_or_else(|e| e.into_inner());
            let terminal = matches!(
                st.status,
                TransferJobStatus::Succeeded
                    | TransferJobStatus::Failed
                    | TransferJobStatus::Cancelled
            );
            if !terminal {
                return true;
            }
            let finished = st.finished_unix_ms.unwrap_or(st.created_unix_ms);
            now.saturating_sub(finished) < JOB_TTL_SECS * 1000
        });
        if map.len() <= MAX_JOBS {
            return;
        }
        let mut terminal: Vec<(String, u64)> = map
            .iter()
            .filter_map(|(id, j)| {
                let st = j.state.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(
                    st.status,
                    TransferJobStatus::Succeeded
                        | TransferJobStatus::Failed
                        | TransferJobStatus::Cancelled
                ) {
                    Some((
                        id.clone(),
                        st.finished_unix_ms.unwrap_or(st.created_unix_ms),
                    ))
                } else {
                    None
                }
            })
            .collect();
        terminal.sort_by_key(|(_, t)| *t);
        let overflow = map.len().saturating_sub(MAX_JOBS);
        for (id, _) in terminal.into_iter().take(overflow) {
            map.remove(&id);
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── session helpers ─────────────────────────────────────────────────────────

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

fn begin_inflight(rt: &SessionRuntime) -> Result<(), ToolError> {
    let inflight = rt.mcp_exec_inflight.fetch_add(1, Ordering::SeqCst);
    if inflight >= MAX_INFLIGHT {
        rt.mcp_exec_inflight.fetch_sub(1, Ordering::SeqCst);
        return Err(ToolError::busy(format!(
            "too many concurrent side-channel ops on this session (max {MAX_INFLIGHT})"
        )));
    }
    Ok(())
}

fn end_inflight(rt: &SessionRuntime) {
    rt.mcp_exec_inflight.fetch_sub(1, Ordering::SeqCst);
}

pub fn resolve_remote_path(remote: &str, cwd: Option<&str>) -> Result<String, ToolError> {
    let p = remote.trim();
    if p.is_empty() {
        return Err(ToolError::invalid("remote_path must not be empty"));
    }
    if p.contains('\0') || p.contains('\n') || p.contains('\r') {
        return Err(ToolError::invalid(
            "remote_path must not contain NUL/CR/LF",
        ));
    }
    if p.starts_with('/') || p.starts_with('~') {
        return Ok(p.to_string());
    }
    if let Some(c) = cwd.map(str::trim).filter(|s| !s.is_empty()) {
        let base = c.trim_end_matches('/');
        return Ok(format!("{base}/{p}"));
    }
    Ok(p.to_string())
}

fn tracked_cwd(rt: &SessionRuntime) -> Option<String> {
    rt.cwd
        .lock()
        .ok()
        .and_then(|t| t.last_known().map(|s| s.to_string()))
}

fn resolve_content_timeout_secs(arg: Option<u64>, cfg: &McpConfig) -> u64 {
    arg.unwrap_or(
        cfg.exec_timeout_secs
            .max(DEFAULT_CONTENT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS),
    )
    .clamp(1, MAX_TIMEOUT_SECS)
}

fn resolve_path_timeout_secs(arg: Option<u64>, cfg: &McpConfig) -> u64 {
    let def = cfg.transfer_timeout_secs.max(1);
    let max = cfg.transfer_max_timeout_secs.max(def);
    arg.unwrap_or(def).clamp(1, max)
}

fn resolve_max_transfer_bytes(arg: Option<u64>) -> usize {
    arg.unwrap_or(DEFAULT_MAX_TRANSFER_BYTES as u64)
        .clamp(1, HARD_MAX_TRANSFER_BYTES as u64) as usize
}

fn decode_content(content: &str, encoding: &str) -> Result<Vec<u8>, ToolError> {
    match encoding.trim().to_ascii_lowercase().as_str() {
        "" | "utf8" | "utf-8" | "text" => Ok(content.as_bytes().to_vec()),
        "base64" | "b64" => base64::engine::general_purpose::STANDARD
            .decode(content.trim().as_bytes())
            .map_err(|e| ToolError::invalid(format!("invalid base64 content: {e}"))),
        other => Err(ToolError::invalid(format!(
            "encoding must be utf8 or base64, got {other}"
        ))),
    }
}

fn encode_content(bytes: &[u8], prefer: &str) -> (String, String) {
    let prefer = prefer.trim().to_ascii_lowercase();
    match prefer.as_str() {
        "base64" | "b64" => (
            base64::engine::general_purpose::STANDARD.encode(bytes),
            "base64".into(),
        ),
        "utf8" | "utf-8" | "text" => match std::str::from_utf8(bytes) {
            Ok(s) => (s.to_string(), "utf8".into()),
            Err(_) => (
                base64::engine::general_purpose::STANDARD.encode(bytes),
                "base64".into(),
            ),
        },
        _ => match std::str::from_utf8(bytes) {
            Ok(s) if !s.chars().any(|c| c == '\0') => (s.to_string(), "utf8".into()),
            _ => (
                base64::engine::general_purpose::STANDARD.encode(bytes),
                "base64".into(),
            ),
        },
    }
}

fn map_transfer_err(e: AppError) -> ToolError {
    super::transfer_util::map_app_error_transfer(e)
}

pub fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn cancelled_err() -> ToolError {
    ToolError::cancelled("transfer cancelled")
}

fn check_cancel(cancel: &Option<Arc<AtomicBool>>) -> Result<(), ToolError> {
    if cancel
        .as_ref()
        .map(|c| c.load(Ordering::SeqCst))
        .unwrap_or(false)
    {
        return Err(cancelled_err());
    }
    Ok(())
}

// ── staging paths ───────────────────────────────────────────────────────────

/// Remote staging path next to final (same directory).
pub fn remote_staging_path(final_path: &str, job_id: &str) -> String {
    let short = &job_id[..job_id.len().min(8)];
    format!("{final_path}.anchorterm-part-{short}")
}

/// Local staging file in the same directory as the final destination.
pub fn local_staging_path(final_path: &Path, job_id: &str) -> PathBuf {
    let short = &job_id[..job_id.len().min(8)];
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    let name = final_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download.bin".into());
    parent.join(format!(".anchorterm-part-{short}-{name}"))
}

// ── hash / size ────────────────────────────────────────────────────────────

fn local_file_size(path: &Path) -> Result<u64, ToolError> {
    std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| ToolError::internal(format!("stat local {}: {e}", path.display())))
}

fn local_sha256_file(path: &Path) -> Result<String, ToolError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| ToolError::internal(format!("open local for sha256: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| ToolError::internal(format!("read local for sha256: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn control_path_of(rt: &SessionRuntime) -> Option<PathBuf> {
    rt.control_path.lock().ok().and_then(|g| g.clone())
}

async fn remote_exec(
    params: &ConnectParams,
    rt: &SessionRuntime,
    command: &str,
    timeout: Duration,
    max_out: usize,
) -> Result<openssh::SideChannelBytesResult, ToolError> {
    let cp = control_path_of(rt);
    openssh::openssh_exec_bytes(
        params,
        command,
        None,
        &rt.side_channel_key,
        cp.as_deref(),
        timeout,
        max_out,
    )
    .await
    .map_err(map_transfer_err)
}

async fn remote_file_size(
    params: &ConnectParams,
    rt: &SessionRuntime,
    remote: &str,
    timeout: Duration,
) -> Result<u64, ToolError> {
    let q = shell_single_quote(remote);
    // Linux stat; fallback wc -c.
    let cmd = format!(
        "if test -f {q}; then (stat -c%s -- {q} 2>/dev/null || wc -c < {q}); else echo MISSING; exit 1; fi"
    );
    let res = remote_exec(params, rt, &cmd, timeout.min(Duration::from_secs(60)), 256).await?;
    if res.exit_code.unwrap_or(1) != 0 {
        return Err(ToolError::file_not_found(format!(
            "remote file missing or unreadable: {remote}: {}",
            res.stderr.trim()
        )));
    }
    let s = String::from_utf8_lossy(&res.stdout);
    let num = s
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>();
    num.parse::<u64>().map_err(|_| {
        ToolError::internal(format!(
            "cannot parse remote size for {remote}: {}",
            s.trim()
        ))
    })
}

async fn remote_sha256(
    params: &ConnectParams,
    rt: &SessionRuntime,
    remote: &str,
    timeout: Duration,
) -> Result<String, ToolError> {
    let q = shell_single_quote(remote);
    // Prefer sha256sum; fall back to shasum / openssl.
    let cmd = format!(
        "H=$(sha256sum -- {q} 2>/dev/null | awk '{{print $1}}'); \
         if [ -z \"$H\" ]; then H=$(shasum -a 256 -- {q} 2>/dev/null | awk '{{print $1}}'); fi; \
         if [ -z \"$H\" ]; then H=$(openssl dgst -sha256 {q} 2>/dev/null | awk '{{print $NF}}'); fi; \
         if [ -z \"$H\" ]; then echo NOHASH; exit 2; fi; printf '%s' \"$H\""
    );
    let res = remote_exec(
        params,
        rt,
        &cmd,
        timeout.min(Duration::from_secs(600)),
        128,
    )
    .await?;
    let out = String::from_utf8_lossy(&res.stdout).trim().to_string();
    if res.exit_code.unwrap_or(1) != 0 || out == "NOHASH" || out.len() < 32 {
        return Err(ToolError::internal(format!(
            "remote sha256 unavailable for {remote} (install sha256sum/shasum/openssl): {}",
            res.stderr.trim()
        )));
    }
    Ok(out.to_ascii_lowercase())
}

async fn remote_rm(
    params: &ConnectParams,
    rt: &SessionRuntime,
    remote: &str,
    timeout: Duration,
) -> bool {
    let q = shell_single_quote(remote);
    let cmd = format!("rm -f -- {q}");
    match remote_exec(params, rt, &cmd, timeout.min(Duration::from_secs(30)), 512).await {
        Ok(r) => r.exit_code.unwrap_or(1) == 0,
        Err(_) => false,
    }
}

async fn remote_mv(
    params: &ConnectParams,
    rt: &SessionRuntime,
    from: &str,
    to: &str,
    timeout: Duration,
) -> Result<(), ToolError> {
    let cmd = format!(
        "mv -f -- {} {}",
        shell_single_quote(from),
        shell_single_quote(to)
    );
    let res = remote_exec(params, rt, &cmd, timeout.min(Duration::from_secs(60)), 1024).await?;
    if res.exit_code.unwrap_or(1) != 0 {
        return Err(ToolError::internal(format!(
            "remote mv failed {} -> {}: {}",
            from,
            to,
            res.stderr.trim()
        )));
    }
    Ok(())
}

async fn prep_remote_final(
    params: &ConnectParams,
    rt: &SessionRuntime,
    remote_final: &str,
    create_dirs: bool,
    overwrite: bool,
    timeout: Duration,
) -> Result<(), ToolError> {
    let q = shell_single_quote(remote_final);
    if create_dirs {
        let mkdir_cmd = format!("mkdir -p -- \"$(dirname -- {q})\"");
        let res =
            remote_exec(params, rt, &mkdir_cmd, timeout.min(Duration::from_secs(120)), 4096)
                .await?;
        if res.exit_code.unwrap_or(1) != 0 {
            return Err(ToolError::internal(format!(
                "mkdir parent failed: {}",
                res.stderr.trim()
            )));
        }
    }
    if !overwrite {
        let check = format!("test ! -e {q}");
        let chk = remote_exec(params, rt, &check, timeout.min(Duration::from_secs(60)), 1024)
            .await?;
        if chk.exit_code.unwrap_or(1) != 0 {
            return Err(ToolError::invalid(format!(
                "remote_path already exists and overwrite=false: {remote_final}"
            )));
        }
    }
    Ok(())
}

// ── verify helpers ──────────────────────────────────────────────────────────

async fn verify_upload(
    params: &ConnectParams,
    rt: &SessionRuntime,
    local: &Path,
    remote_stage: &str,
    expected_bytes: u64,
    verify: VerifyMode,
    timeout: Duration,
    warnings: &mut Vec<String>,
) -> Result<(bool, Option<String>), ToolError> {
    match verify {
        VerifyMode::None => Ok((false, None)),
        VerifyMode::Size => {
            let remote_sz = remote_file_size(params, rt, remote_stage, timeout).await?;
            if remote_sz != expected_bytes {
                return Err(ToolError::verify_failed(format!(
                    "size verify failed: local={expected_bytes} remote_stage={remote_sz}"
                )));
            }
            Ok((true, None))
        }
        VerifyMode::Sha256 => {
            let local_h = local_sha256_file(local)?;
            match remote_sha256(params, rt, remote_stage, timeout).await {
                Ok(remote_h) => {
                    if remote_h != local_h {
                        return Err(ToolError::verify_failed(format!(
                            "sha256 verify failed: local={local_h} remote={remote_h}"
                        )));
                    }
                    // also check size for extra safety
                    let remote_sz = remote_file_size(params, rt, remote_stage, timeout).await?;
                    if remote_sz != expected_bytes {
                        return Err(ToolError::verify_failed(format!(
                            "size mismatch after sha256 ok: local={expected_bytes} remote={remote_sz}"
                        )));
                    }
                    Ok((true, Some(local_h)))
                }
                Err(e) => {
                    warnings.push(format!(
                        "sha256 verify unavailable ({}); falling back to size",
                        e.message
                    ));
                    let remote_sz = remote_file_size(params, rt, remote_stage, timeout).await?;
                    if remote_sz != expected_bytes {
                        return Err(ToolError::verify_failed(format!(
                            "size verify failed (sha256 fallback): local={expected_bytes} remote={remote_sz}"
                        )));
                    }
                    Ok((true, Some(local_h)))
                }
            }
        }
    }
}

async fn verify_download(
    params: &ConnectParams,
    rt: &SessionRuntime,
    local_stage: &Path,
    remote: &str,
    expected_remote: Option<u64>,
    verify: VerifyMode,
    timeout: Duration,
    warnings: &mut Vec<String>,
) -> Result<(bool, u64, Option<String>), ToolError> {
    let local_sz = local_file_size(local_stage)?;
    match verify {
        VerifyMode::None => Ok((false, local_sz, None)),
        VerifyMode::Size => {
            let remote_sz = match expected_remote {
                Some(s) => s,
                None => remote_file_size(params, rt, remote, timeout).await?,
            };
            if local_sz != remote_sz {
                return Err(ToolError::verify_failed(format!(
                    "size verify failed: local_stage={local_sz} remote={remote_sz}"
                )));
            }
            Ok((true, local_sz, None))
        }
        VerifyMode::Sha256 => {
            let local_h = local_sha256_file(local_stage)?;
            match remote_sha256(params, rt, remote, timeout).await {
                Ok(remote_h) => {
                    if remote_h != local_h {
                        return Err(ToolError::verify_failed(format!(
                            "sha256 verify failed: local={local_h} remote={remote_h}"
                        )));
                    }
                    Ok((true, local_sz, Some(local_h)))
                }
                Err(e) => {
                    warnings.push(format!(
                        "sha256 verify unavailable ({}); falling back to size",
                        e.message
                    ));
                    let remote_sz = match expected_remote {
                        Some(s) => s,
                        None => remote_file_size(params, rt, remote, timeout).await?,
                    };
                    if local_sz != remote_sz {
                        return Err(ToolError::verify_failed(format!(
                            "size verify failed (sha256 fallback): local={local_sz} remote={remote_sz}"
                        )));
                    }
                    Ok((true, local_sz, Some(local_h)))
                }
            }
        }
    }
}

// ── path transfer (atomic + verify + cleanup) ───────────────────────────────

struct PathTransferOpts {
    direction: TransferDirection,
    local_final: PathBuf,
    remote_final: String,
    job_id: String,
    expected_bytes: Option<u64>,
    create_dirs: bool,
    overwrite: bool,
    verify: VerifyMode,
    cleanup_on_fail: bool,
    timeout: Duration,
    /// When set with progress_job, enables staging size polling during scp.
    progress_rt: Option<Arc<SessionRuntime>>,
    recursive: bool,
    prefer_rsync: bool,
    max_retries: u32,
    retry_backoff: Duration,
    progress_poll_secs: u64,
}

/// Copy local→remote or remote→local with optional rsync + retries.
async fn run_transport(
    params: &ConnectParams,
    rt: &SessionRuntime,
    direction: TransferDirection,
    local: &Path,
    remote: &str,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
    recursive: bool,
    prefer_rsync: bool,
    max_retries: u32,
    retry_backoff: Duration,
    warnings: &mut Vec<String>,
) -> Result<String, ToolError> {
    // Returns transport name used: "rsync" | "scp"
    let mut last_err: Option<ToolError> = None;
    let attempts = max_retries.saturating_add(1).max(1);
    let rsync_available = prefer_rsync && openssh::find_rsync().is_some();

    ops_log::log(
        "MCP",
        &format!(
            "transfer transport begin dir={:?} recursive={recursive} prefer_rsync={prefer_rsync} rsync_available={rsync_available} attempts={attempts} timeout_s={} local={} remote_len={}",
            direction,
            timeout.as_secs(),
            local.display(),
            remote.len()
        ),
    );

    for attempt in 0..attempts {
        if attempt > 0 {
            let prev = last_err
                .as_ref()
                .map(|e| e.code)
                .unwrap_or("error");
            warnings.push(format!(
                "retry {}/{} after {prev}",
                attempt, max_retries
            ));
            ops_log::log(
                "MCP",
                &format!(
                    "transfer transport retry attempt={attempt}/{max_retries} after={prev} backoff_ms={}",
                    retry_backoff.as_millis()
                ),
            );
            tokio::time::sleep(retry_backoff).await;
            if cancel
                .as_ref()
                .map(|c| c.load(Ordering::SeqCst))
                .unwrap_or(false)
            {
                ops_log::log("MCP", "transfer transport cancelled during backoff");
                return Err(cancelled_err());
            }
        }

        // Prefer rsync when available.
        if rsync_available {
            ops_log::log(
                "MCP",
                &format!("transfer transport try=rsync attempt={attempt}"),
            );
            let r = match direction {
                TransferDirection::Upload => {
                    openssh::openssh_rsync_upload(
                        params,
                        local,
                        remote,
                        &rt.side_channel_key,
                        control_path_of(rt).as_deref(),
                        timeout,
                        cancel.clone(),
                        recursive,
                        true, // partial resume
                    )
                    .await
                }
                TransferDirection::Download => {
                    openssh::openssh_rsync_download(
                        params,
                        remote,
                        local,
                        &rt.side_channel_key,
                        control_path_of(rt).as_deref(),
                        timeout,
                        cancel.clone(),
                        recursive,
                        true,
                    )
                    .await
                }
            };
            match r {
                Ok(_) => {
                    if attempt > 0 {
                        warnings.push("retry succeeded via rsync".into());
                    }
                    ops_log::log(
                        "MCP",
                        &format!("transfer transport ok=rsync attempt={attempt}"),
                    );
                    return Ok("rsync".into());
                }
                Err(e) => {
                    let te = map_transfer_err(e);
                    ops_log::log(
                        "MCP",
                        &format!(
                            "transfer transport rsync fail attempt={attempt} code={} retryable={} msg={}",
                            te.code, te.retryable, te.message
                        ),
                    );
                    if te.code == "cancelled" {
                        return Err(te);
                    }
                    // Fall through to scp on first attempt if rsync hard-missing semantics
                    if te.message.contains("不可用") {
                        warnings.push("rsync unavailable; using scp".into());
                    } else if super::transfer_util::is_retryable_transfer_err(&te)
                        && attempt + 1 < attempts
                    {
                        last_err = Some(te);
                        continue;
                    } else {
                        // Fall back to scp on this attempt when rsync failed.
                        warnings.push(format!(
                            "rsync failed ({}); falling back to scp",
                            te.code
                        ));
                    }
                }
            }
        }

        ops_log::log(
            "MCP",
            &format!("transfer transport try=scp attempt={attempt}"),
        );
        let r = match direction {
            TransferDirection::Upload => {
                openssh::openssh_scp_upload(
                    params,
                    local,
                    remote,
                    &rt.side_channel_key,
                    control_path_of(rt).as_deref(),
                    timeout,
                    cancel.clone(),
                    recursive,
                )
                .await
            }
            TransferDirection::Download => {
                openssh::openssh_scp_download(
                    params,
                    remote,
                    local,
                    &rt.side_channel_key,
                    control_path_of(rt).as_deref(),
                    timeout,
                    cancel.clone(),
                    recursive,
                )
                .await
            }
        };
        match r {
            Ok(_) => {
                if attempt > 0 {
                    warnings.push("retry succeeded via scp".into());
                }
                ops_log::log(
                    "MCP",
                    &format!("transfer transport ok=scp attempt={attempt}"),
                );
                return Ok("scp".into());
            }
            Err(e) => {
                let te = map_transfer_err(e);
                ops_log::log(
                    "MCP",
                    &format!(
                        "transfer transport scp fail attempt={attempt} code={} retryable={} msg={}",
                        te.code, te.retryable, te.message
                    ),
                );
                if te.code == "cancelled" {
                    return Err(te);
                }
                if super::transfer_util::is_retryable_transfer_err(&te) && attempt + 1 < attempts {
                    last_err = Some(te);
                    continue;
                }
                return Err(te);
            }
        }
    }
    let err =
        last_err.unwrap_or_else(|| ToolError::transfer_failed("transfer failed after retries"));
    ops_log::log(
        "MCP",
        &format!(
            "transfer transport exhausted attempts={} last_code={}",
            attempts, err.code
        ),
    );
    Err(err)
}

/// Progress poll target while scp is in flight.
enum StageProgressTarget {
    /// Upload: stat remote staging path via side-channel ssh.
    RemoteStage(String),
    /// Download: stat local staging path on disk.
    LocalStage(PathBuf),
}

/// Poll staging size until `stop` is set; updates job progress fields.
fn spawn_progress_poller(
    job: Arc<TransferJob>,
    params: ConnectParams,
    rt: Arc<SessionRuntime>,
    target: StageProgressTarget,
    bytes_total: Option<u64>,
    cancel: Option<Arc<AtomicBool>>,
    stop: Arc<AtomicBool>,
    poll_secs: u64,
) -> tokio::task::JoinHandle<()> {
    let poll_secs = poll_secs.clamp(1, 60);
    tokio::spawn(async move {
        let mut last_bytes: u64 = 0;
        let mut last_at = Instant::now();
        let source = match &target {
            StageProgressTarget::RemoteStage(_) => "remote_stage_stat",
            StageProgressTarget::LocalStage(_) => "local_stage_stat",
        };
        // Seed phase so status shows transferring even before first sample.
        set_job_progress(
            &job,
            "transferring",
            Some(0),
            bytes_total,
            None,
            Some(source),
        );

        while !stop.load(Ordering::SeqCst) {
            if cancel
                .as_ref()
                .map(|c| c.load(Ordering::SeqCst))
                .unwrap_or(false)
            {
                break;
            }

            let sample = match &target {
                StageProgressTarget::RemoteStage(path) => {
                    // Short timeout: progress must not block forever if ssh is busy.
                    remote_file_size(
                        &params,
                        &rt,
                        path,
                        Duration::from_secs(15),
                    )
                    .await
                    .ok()
                }
                StageProgressTarget::LocalStage(path) => {
                    if path.is_file() {
                        local_file_size(path).ok()
                    } else if path.is_dir() {
                        // Recursive download: report dir presence only (0 until files appear).
                        Some(dir_size_approx(path))
                    } else {
                        Some(0)
                    }
                }
            };

            if let Some(sz) = sample {
                let elapsed = last_at.elapsed().as_secs_f64();
                let bps = if elapsed >= 0.5 && sz >= last_bytes {
                    let delta = sz - last_bytes;
                    Some((delta as f64 / elapsed).round() as u64)
                } else {
                    None
                };
                // Never report > total (scp may briefly overshoot on sparse metadata).
                let capped = match bytes_total {
                    Some(t) if t > 0 => sz.min(t),
                    _ => sz,
                };
                set_job_progress(
                    &job,
                    "transferring",
                    Some(capped),
                    bytes_total,
                    bps,
                    Some(source),
                );
                last_bytes = sz;
                last_at = Instant::now();
            }

            // Sleep in small slices so stop/cancel is responsive.
            let step = Duration::from_millis(250);
            let mut waited = Duration::ZERO;
            let interval = Duration::from_secs(poll_secs);
            while waited < interval && !stop.load(Ordering::SeqCst) {
                tokio::time::sleep(step).await;
                waited += step;
            }
        }
    })
}

/// Best-effort directory size (shallow walk, capped) for progress display.
fn dir_size_approx(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    let mut n = 0usize;
    while let Some(p) = stack.pop() {
        n += 1;
        if n > 5000 {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for ent in rd.flatten() {
            let Ok(meta) = ent.metadata() else {
                continue;
            };
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            } else if meta.is_dir() {
                stack.push(ent.path());
            }
        }
    }
    total
}

/// Returns (result, cleaned_up_flag). cleaned_up is true when staging was removed.
async fn execute_path_transfer(
    rt: &SessionRuntime,
    params: &ConnectParams,
    opts: &PathTransferOpts,
    cancel: Option<Arc<AtomicBool>>,
    progress_job: Option<Arc<TransferJob>>,
) -> Result<TransferComplete, (ToolError, bool)> {
    let mut warnings = Vec::new();
    let short_timeout = opts.timeout.min(Duration::from_secs(120));

    ops_log::log(
        "MCP",
        &format!(
            "transfer path begin job={} dir={:?} recursive={} verify={} prefer_rsync={} retries={} timeout_s={} local={} remote_len={} expected={:?}",
            &opts.job_id[..opts.job_id.len().min(8)],
            opts.direction,
            opts.recursive,
            opts.verify.as_str(),
            opts.prefer_rsync,
            opts.max_retries,
            opts.timeout.as_secs(),
            opts.local_final.display(),
            opts.remote_final.len(),
            opts.expected_bytes
        ),
    );

    if let Some(ref job) = progress_job {
        set_job_progress(job, "preparing", Some(0), opts.expected_bytes, None, None);
    }

    match opts.direction {
        TransferDirection::Upload => {
            let expected = match opts.expected_bytes {
                Some(b) => b,
                None => local_file_size(&opts.local_final).map_err(|e| (e, false))?,
            };
            let remote_stage = remote_staging_path(&opts.remote_final, &opts.job_id);
            ops_log::log(
                "MCP",
                &format!(
                    "transfer upload prepare expected={expected} stage_len={} final_len={}",
                    remote_stage.len(),
                    opts.remote_final.len()
                ),
            );

            // Always try to remove stale stage from a previous crash.
            let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;

            if let Err(e) = prep_remote_final(
                params,
                rt,
                &opts.remote_final,
                opts.create_dirs,
                opts.overwrite,
                opts.timeout,
            )
            .await
            {
                return Err((e, false));
            }
            if let Err(e) = check_cancel(&cancel) {
                return Err((e, true));
            }

            let stop_progress = Arc::new(AtomicBool::new(false));
            let poll_handle = match (&progress_job, &opts.progress_rt) {
                (Some(job), Some(rt_arc)) if !opts.recursive => Some(spawn_progress_poller(
                    Arc::clone(job),
                    params.clone(),
                    Arc::clone(rt_arc),
                    StageProgressTarget::RemoteStage(remote_stage.clone()),
                    Some(expected),
                    cancel.clone(),
                    Arc::clone(&stop_progress),
                    opts.progress_poll_secs,
                )),
                _ => None,
            };

            let transport = if opts.recursive {
                // Directory trees: write directly to final (no atomic staging).
                run_transport(
                    params,
                    rt,
                    TransferDirection::Upload,
                    &opts.local_final,
                    &opts.remote_final,
                    opts.timeout,
                    cancel.clone(),
                    true,
                    opts.prefer_rsync,
                    opts.max_retries,
                    opts.retry_backoff,
                    &mut warnings,
                )
                .await
            } else {
                run_transport(
                    params,
                    rt,
                    TransferDirection::Upload,
                    &opts.local_final,
                    &remote_stage,
                    opts.timeout,
                    cancel.clone(),
                    false,
                    opts.prefer_rsync,
                    opts.max_retries,
                    opts.retry_backoff,
                    &mut warnings,
                )
                .await
            };

            stop_progress.store(true, Ordering::SeqCst);
            if let Some(h) = poll_handle {
                let _ = h.await;
            }

            if let Err(e) = transport {
                let cleaned = if opts.cleanup_on_fail && !opts.recursive {
                    let c = remote_rm(params, rt, &remote_stage, short_timeout).await;
                    ops_log::log(
                        "MCP",
                        &format!(
                            "transfer upload fail cleanup_stage cleaned={c} code={}",
                            e.code
                        ),
                    );
                    c
                } else {
                    ops_log::log(
                        "MCP",
                        &format!("transfer upload fail code={} (no stage cleanup)", e.code),
                    );
                    false
                };
                return Err((e, cleaned));
            } else if let Ok(ref name) = transport {
                warnings.push(format!("transport={name}"));
            }
            if let Err(e) = check_cancel(&cancel) {
                let cleaned = if opts.cleanup_on_fail && !opts.recursive {
                    remote_rm(params, rt, &remote_stage, short_timeout).await
                } else {
                    false
                };
                ops_log::log("MCP", "transfer upload cancelled after transport");
                return Err((e, cleaned));
            }

            if opts.recursive {
                // Skip per-file verify for trees.
                warnings.push("recursive=true: atomic staging and sha256 verify skipped".into());
                if let Some(ref job) = progress_job {
                    set_job_progress(job, "done", opts.expected_bytes, opts.expected_bytes, None, None);
                }
                ops_log::log(
                    "MCP",
                    &format!("transfer upload recursive done bytes={expected}"),
                );
                return Ok(TransferComplete {
                    bytes: expected,
                    verified: false,
                    verify_mode: VerifyMode::None,
                    sha256: None,
                    warnings,
                });
            }

            if let Some(ref job) = progress_job {
                set_job_progress(
                    job,
                    "verifying",
                    Some(expected),
                    Some(expected),
                    None,
                    Some("remote_stage_stat"),
                );
            }
            ops_log::log(
                "MCP",
                &format!(
                    "transfer upload verify begin mode={} expected={expected}",
                    opts.verify.as_str()
                ),
            );

            let verify_res = verify_upload(
                params,
                rt,
                &opts.local_final,
                &remote_stage,
                expected,
                opts.verify,
                opts.timeout,
                &mut warnings,
            )
            .await;

            match verify_res {
                Ok((verified, sha)) => {
                    ops_log::log(
                        "MCP",
                        &format!(
                            "transfer upload verify ok verified={verified} sha={}",
                            sha.as_deref().unwrap_or("-")
                        ),
                    );
                    if let Some(ref job) = progress_job {
                        set_job_progress(
                            job,
                            "finalizing",
                            Some(expected),
                            Some(expected),
                            None,
                            None,
                        );
                    }
                    if let Err(e) = remote_mv(
                        params,
                        rt,
                        &remote_stage,
                        &opts.remote_final,
                        short_timeout,
                    )
                    .await
                    {
                        let cleaned = if opts.cleanup_on_fail {
                            remote_rm(params, rt, &remote_stage, short_timeout).await
                        } else {
                            false
                        };
                        ops_log::log(
                            "MCP",
                            &format!("transfer upload mv fail cleaned={cleaned} err={}", e.message),
                        );
                        return Err((e, cleaned));
                    }
                    warnings.push("atomic=remote_stage+mv".into());
                    warnings.push(format!(
                        "progress_poll={}s source=remote_stage_stat",
                        opts.progress_poll_secs
                    ));
                    ops_log::log(
                        "MCP",
                        &format!(
                            "transfer upload done bytes={expected} verified={verified} atomic=1"
                        ),
                    );
                    Ok(TransferComplete {
                        bytes: expected,
                        verified,
                        verify_mode: opts.verify,
                        sha256: sha,
                        warnings,
                    })
                }
                Err(e) => {
                    let cleaned = if opts.cleanup_on_fail {
                        remote_rm(params, rt, &remote_stage, short_timeout).await
                    } else {
                        false
                    };
                    ops_log::log(
                        "MCP",
                        &format!(
                            "transfer upload verify fail code={} cleaned={cleaned} msg={}",
                            e.code, e.message
                        ),
                    );
                    Err((e, cleaned))
                }
            }
        }
        TransferDirection::Download => {
            let local_stage = local_staging_path(&opts.local_final, &opts.job_id);
            // Remove stale local stage.
            let _ = std::fs::remove_file(&local_stage);

            if let Some(parent) = opts.local_final.parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        return Err((
                            ToolError::internal(format!(
                                "create local parent {}: {e}",
                                parent.display()
                            )),
                            false,
                        ));
                    }
                }
            }

            // Best-effort remote size for verify + job bytes_total (files only).
            let remote_sz = if opts.recursive {
                None
            } else {
                match remote_file_size(params, rt, &opts.remote_final, short_timeout).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        return Err((e, false));
                    }
                }
            };

            if let Some(ref job) = progress_job {
                set_job_progress(job, "preparing", Some(0), remote_sz, None, None);
            }

            if let Err(e) = check_cancel(&cancel) {
                return Err((e, true));
            }

            let stop_progress = Arc::new(AtomicBool::new(false));
            let poll_handle = match (&progress_job, &opts.progress_rt) {
                (Some(job), Some(rt_arc)) => Some(spawn_progress_poller(
                    Arc::clone(job),
                    params.clone(),
                    Arc::clone(rt_arc),
                    StageProgressTarget::LocalStage(if opts.recursive {
                        opts.local_final.clone()
                    } else {
                        local_stage.clone()
                    }),
                    remote_sz,
                    cancel.clone(),
                    Arc::clone(&stop_progress),
                    opts.progress_poll_secs,
                )),
                _ => None,
            };

            let transport = if opts.recursive {
                run_transport(
                    params,
                    rt,
                    TransferDirection::Download,
                    &opts.local_final,
                    &opts.remote_final,
                    opts.timeout,
                    cancel.clone(),
                    true,
                    opts.prefer_rsync,
                    opts.max_retries,
                    opts.retry_backoff,
                    &mut warnings,
                )
                .await
            } else {
                run_transport(
                    params,
                    rt,
                    TransferDirection::Download,
                    &local_stage,
                    &opts.remote_final,
                    opts.timeout,
                    cancel.clone(),
                    false,
                    opts.prefer_rsync,
                    opts.max_retries,
                    opts.retry_backoff,
                    &mut warnings,
                )
                .await
            };

            stop_progress.store(true, Ordering::SeqCst);
            if let Some(h) = poll_handle {
                let _ = h.await;
            }

            if let Err(e) = transport {
                let cleaned = if opts.cleanup_on_fail && !opts.recursive {
                    let _ = std::fs::remove_file(&local_stage);
                    true
                } else {
                    false
                };
                ops_log::log(
                    "MCP",
                    &format!(
                        "transfer download fail code={} cleaned={cleaned} msg={}",
                        e.code, e.message
                    ),
                );
                return Err((e, cleaned));
            } else if let Ok(ref name) = transport {
                warnings.push(format!("transport={name}"));
            }
            if let Err(e) = check_cancel(&cancel) {
                let cleaned = if opts.cleanup_on_fail && !opts.recursive {
                    let _ = std::fs::remove_file(&local_stage);
                    true
                } else {
                    false
                };
                ops_log::log("MCP", "transfer download cancelled after transport");
                return Err((e, cleaned));
            }

            if opts.recursive {
                warnings.push("recursive=true: atomic staging and sha256 verify skipped".into());
                let bytes = dir_size_approx(&opts.local_final);
                ops_log::log(
                    "MCP",
                    &format!("transfer download recursive done bytes={bytes}"),
                );
                return Ok(TransferComplete {
                    bytes,
                    verified: false,
                    verify_mode: VerifyMode::None,
                    sha256: None,
                    warnings,
                });
            }

            if let Some(ref job) = progress_job {
                let done = local_file_size(&local_stage).ok();
                set_job_progress(
                    job,
                    "verifying",
                    done.or(remote_sz),
                    remote_sz,
                    None,
                    Some("local_stage_stat"),
                );
            }
            ops_log::log(
                "MCP",
                &format!(
                    "transfer download verify begin mode={} remote_sz={:?}",
                    opts.verify.as_str(),
                    remote_sz
                ),
            );

            let verify_res = verify_download(
                params,
                rt,
                &local_stage,
                &opts.remote_final,
                remote_sz,
                opts.verify,
                opts.timeout,
                &mut warnings,
            )
            .await;

            match verify_res {
                Ok((verified, bytes, sha)) => {
                    ops_log::log(
                        "MCP",
                        &format!(
                            "transfer download verify ok verified={verified} bytes={bytes} sha={}",
                            sha.as_deref().unwrap_or("-")
                        ),
                    );
                    if let Some(ref job) = progress_job {
                        set_job_progress(
                            job,
                            "finalizing",
                            Some(bytes),
                            Some(bytes),
                            None,
                            None,
                        );
                    }
                    // Atomic-ish promote: remove dest then rename stage → final.
                    if opts.local_final.exists() {
                        if !opts.overwrite {
                            let _ = std::fs::remove_file(&local_stage);
                            return Err((
                                ToolError::invalid(format!(
                                    "local_path exists and overwrite=false: {}",
                                    opts.local_final.display()
                                )),
                                true,
                            ));
                        }
                        let _ = std::fs::remove_file(&opts.local_final);
                    }
                    if let Err(e) = std::fs::rename(&local_stage, &opts.local_final) {
                        match std::fs::copy(&local_stage, &opts.local_final) {
                            Ok(_) => {
                                let _ = std::fs::remove_file(&local_stage);
                            }
                            Err(e2) => {
                                let cleaned = if opts.cleanup_on_fail {
                                    let _ = std::fs::remove_file(&local_stage);
                                    true
                                } else {
                                    false
                                };
                                ops_log::log(
                                    "MCP",
                                    &format!(
                                        "transfer download promote fail cleaned={cleaned} rename={e} copy={e2}"
                                    ),
                                );
                                return Err((
                                    ToolError::internal(format!(
                                        "promote local stage failed rename={e} copy={e2}"
                                    )),
                                    cleaned,
                                ));
                            }
                        }
                    }
                    warnings.push("atomic=local_stage+rename".into());
                    warnings.push(format!(
                        "progress_poll={}s source=local_stage_stat",
                        opts.progress_poll_secs
                    ));
                    ops_log::log(
                        "MCP",
                        &format!(
                            "transfer download done bytes={bytes} verified={verified} atomic=1"
                        ),
                    );
                    Ok(TransferComplete {
                        bytes,
                        verified,
                        verify_mode: opts.verify,
                        sha256: sha,
                        warnings,
                    })
                }
                Err(e) => {
                    let cleaned = if opts.cleanup_on_fail {
                        let _ = std::fs::remove_file(&local_stage);
                        true
                    } else {
                        false
                    };
                    ops_log::log(
                        "MCP",
                        &format!(
                            "transfer download verify fail code={} cleaned={cleaned} msg={}",
                            e.code, e.message
                        ),
                    );
                    Err((e, cleaned))
                }
            }
        }
    }
}

// ── content atomic upload ───────────────────────────────────────────────────

async fn execute_content_upload(
    rt: &SessionRuntime,
    params: &ConnectParams,
    remote_final: &str,
    data: &[u8],
    create_dirs: bool,
    overwrite: bool,
    verify: VerifyMode,
    cleanup_on_fail: bool,
    timeout: Duration,
    job_id: &str,
) -> Result<TransferComplete, ToolError> {
    let mut warnings = Vec::new();
    let remote_stage = remote_staging_path(remote_final, job_id);
    let short_timeout = timeout.min(Duration::from_secs(120));
    let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;

    prep_remote_final(
        params,
        rt,
        remote_final,
        create_dirs,
        overwrite,
        timeout,
    )
    .await?;

    let q_stage = shell_single_quote(&remote_stage);
    // Write only to stage (binary-safe cat).
    let write_cmd = format!("cat > {q_stage}");
    let cp = control_path_of(rt);
    let res = openssh::openssh_exec_bytes(
        params,
        &write_cmd,
        Some(data),
        &rt.side_channel_key,
        cp.as_deref(),
        timeout,
        16 * 1024,
    )
    .await
    .map_err(map_transfer_err);

    let res = match res {
        Ok(r) if r.exit_code.unwrap_or(1) == 0 => r,
        Ok(r) => {
            if cleanup_on_fail {
                let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;
            }
            return Err(ToolError::internal(format!(
                "remote stage write failed: {}",
                r.stderr.trim()
            )));
        }
        Err(e) => {
            if cleanup_on_fail {
                let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;
            }
            return Err(e);
        }
    };
    let _ = res;

    // Verify against in-memory content (write a temp local hash path only for size).
    // For size: compare remote stage size to data.len().
    // For sha256: hash data + remote.
    let expected = data.len() as u64;
    let (verified, sha) = match verify {
        VerifyMode::None => (false, None),
        VerifyMode::Size => {
            match remote_file_size(params, rt, &remote_stage, short_timeout).await {
                Ok(sz) if sz == expected => (true, None),
                Ok(sz) => {
                    if cleanup_on_fail {
                        let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;
                    }
                    return Err(ToolError::internal(format!(
                        "size verify failed: content={expected} remote_stage={sz}"
                    )));
                }
                Err(e) => {
                    if cleanup_on_fail {
                        let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;
                    }
                    return Err(e);
                }
            }
        }
        VerifyMode::Sha256 => {
            let local_h = sha256_bytes(data);
            match remote_sha256(params, rt, &remote_stage, timeout).await {
                Ok(remote_h) if remote_h == local_h => (true, Some(local_h)),
                Ok(remote_h) => {
                    if cleanup_on_fail {
                        let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;
                    }
                    return Err(ToolError::internal(format!(
                        "sha256 verify failed: local={local_h} remote={remote_h}"
                    )));
                }
                Err(e) => {
                    warnings.push(format!(
                        "sha256 verify unavailable ({}); falling back to size",
                        e.message
                    ));
                    match remote_file_size(params, rt, &remote_stage, short_timeout).await {
                        Ok(sz) if sz == expected => (true, Some(local_h)),
                        Ok(sz) => {
                            if cleanup_on_fail {
                                let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;
                            }
                            return Err(ToolError::internal(format!(
                                "size verify failed (sha256 fallback): content={expected} remote={sz}"
                            )));
                        }
                        Err(e2) => {
                            if cleanup_on_fail {
                                let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;
                            }
                            return Err(e2);
                        }
                    }
                }
            }
        }
    };

    if let Err(e) = remote_mv(params, rt, &remote_stage, remote_final, short_timeout).await {
        if cleanup_on_fail {
            let _ = remote_rm(params, rt, &remote_stage, short_timeout).await;
        }
        return Err(e);
    }

    warnings.push("atomic=remote_stage+mv".into());
    Ok(TransferComplete {
        bytes: expected,
        verified,
        verify_mode: verify,
        sha256: sha,
        warnings,
    })
}

// ── async job spawn ─────────────────────────────────────────────────────────

struct SpawnOpts {
    direction: TransferDirection,
    local: PathBuf,
    remote: String,
    bytes_total: Option<u64>,
    create_dirs: bool,
    overwrite: bool,
    verify: VerifyMode,
    cleanup_on_fail: bool,
    timeout: Duration,
    warnings: Vec<String>,
    recursive: bool,
    prefer_rsync: bool,
    max_retries: u32,
    retry_backoff: Duration,
    progress_poll_secs: u64,
}

fn spawn_path_job(
    registry: &JobRegistry,
    rt: Arc<SessionRuntime>,
    opts: SpawnOpts,
) -> Result<TransferJobSnapshot, ToolError> {
    require_connected(&rt)?;
    let params = connect_params_from_rt(&rt)?;
    let job_id = Uuid::new_v4().to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    let created = now_unix_ms();
    let job = Arc::new(TransferJob {
        id: job_id.clone(),
        session_id: rt.id.clone(),
        direction: opts.direction,
        local_path: opts.local.display().to_string(),
        remote_path: opts.remote.clone(),
        cancel: Arc::clone(&cancel),
        state: Mutex::new(TransferJobState {
            status: TransferJobStatus::Queued,
            phase: Some("queued".into()),
            bytes_total: opts.bytes_total,
            bytes_transferred: Some(0),
            percent: None,
            bytes_per_sec: None,
            updated_unix_ms: Some(created),
            progress_source: None,
            error: None,
            warnings: opts.warnings.clone(),
            verified: None,
            verify_mode: Some(opts.verify.as_str().into()),
            sha256: None,
            cleaned_up: None,
            created_unix_ms: created,
            started_unix_ms: None,
            finished_unix_ms: None,
        }),
    });
    registry.insert(Arc::clone(&job));
    ops_log::log(
        "MCP",
        &format!(
            "transfer job spawn id={} dir={:?} recursive={} verify={} prefer_rsync={} retries={} poll_s={} local={} remote_len={} bytes_total={:?}",
            &job_id[..job_id.len().min(8)],
            opts.direction,
            opts.recursive,
            opts.verify.as_str(),
            opts.prefer_rsync,
            opts.max_retries,
            opts.progress_poll_secs,
            opts.local.display(),
            opts.remote.len(),
            opts.bytes_total
        ),
    );

    let job_run = Arc::clone(&job);
    let plan = PathTransferOpts {
        direction: opts.direction,
        local_final: opts.local,
        remote_final: opts.remote,
        job_id: job_id.clone(),
        expected_bytes: opts.bytes_total,
        create_dirs: opts.create_dirs,
        overwrite: opts.overwrite,
        verify: opts.verify,
        cleanup_on_fail: opts.cleanup_on_fail,
        timeout: opts.timeout,
        progress_rt: Some(Arc::clone(&rt)),
        recursive: opts.recursive,
        prefer_rsync: opts.prefer_rsync,
        max_retries: opts.max_retries,
        retry_backoff: opts.retry_backoff,
        progress_poll_secs: opts.progress_poll_secs,
    };
    let mut seed_warnings = opts.warnings;

    tokio::spawn(async move {
        if job_run.cancel.load(Ordering::SeqCst) {
            let mut st = job_run.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.status == TransferJobStatus::Queued {
                st.status = TransferJobStatus::Cancelled;
                st.phase = Some("cancelled".into());
                st.finished_unix_ms = Some(now_unix_ms());
                st.updated_unix_ms = Some(now_unix_ms());
                st.error = Some("cancelled".into());
                st.cleaned_up = Some(true);
            }
            return;
        }

        {
            let mut st = job_run.state.lock().unwrap_or_else(|e| e.into_inner());
            st.status = TransferJobStatus::Running;
            st.phase = Some("preparing".into());
            st.started_unix_ms = Some(now_unix_ms());
            st.updated_unix_ms = Some(now_unix_ms());
        }

        if let Err(e) = begin_inflight(&rt) {
            let mut st = job_run.state.lock().unwrap_or_else(|e| e.into_inner());
            st.status = TransferJobStatus::Failed;
            st.phase = Some("failed".into());
            st.error = Some(e.message);
            st.finished_unix_ms = Some(now_unix_ms());
            st.updated_unix_ms = Some(now_unix_ms());
            return;
        }

        let outcome = execute_path_transfer(
            &rt,
            &params,
            &plan,
            Some(Arc::clone(&cancel)),
            Some(Arc::clone(&job_run)),
        )
        .await;

        end_inflight(&rt);

        let mut st = job_run.state.lock().unwrap_or_else(|e| e.into_inner());
        st.finished_unix_ms = Some(now_unix_ms());
        st.updated_unix_ms = Some(now_unix_ms());
        match outcome {
            Ok(done) => {
                st.status = if job_run.cancel.load(Ordering::SeqCst) {
                    TransferJobStatus::Cancelled
                } else {
                    TransferJobStatus::Succeeded
                };
                st.phase = Some(if st.status == TransferJobStatus::Succeeded {
                    "done".into()
                } else {
                    "cancelled".into()
                });
                st.bytes_transferred = Some(done.bytes);
                if st.bytes_total.is_none() {
                    st.bytes_total = Some(done.bytes);
                }
                if let Some(t) = st.bytes_total {
                    if t > 0 && st.status == TransferJobStatus::Succeeded {
                        st.percent = Some(100.0);
                    }
                }
                st.verified = Some(done.verified);
                st.verify_mode = Some(done.verify_mode.as_str().into());
                st.sha256 = done.sha256;
                st.warnings.append(&mut seed_warnings);
                st.warnings.extend(done.warnings);
                if st.status == TransferJobStatus::Cancelled {
                    st.error = Some("cancelled".into());
                }
                st.cleaned_up = Some(false);
            }
            Err((e, cleaned)) => {
                if e.code == "cancelled" || job_run.cancel.load(Ordering::SeqCst) {
                    st.status = TransferJobStatus::Cancelled;
                    st.phase = Some("cancelled".into());
                } else {
                    st.status = TransferJobStatus::Failed;
                    st.phase = Some("failed".into());
                }
                st.error = Some(e.message);
                st.cleaned_up = Some(cleaned);
                st.warnings.append(&mut seed_warnings);
            }
        }

        ops_log::log(
            "MCP",
            &format!(
                "transfer job={} status={:?} phase={:?} sid={} dir={:?} bytes={:?}/{:?} verified={:?} cleaned={:?}",
                &job_run.id[..job_run.id.len().min(8)],
                st.status,
                st.phase,
                &job_run.session_id[..job_run.session_id.len().min(8)],
                plan.direction,
                st.bytes_transferred,
                st.bytes_total,
                st.verified,
                st.cleaned_up
            ),
        );
    });

    Ok(job.snapshot())
}

// ── public upload / download ────────────────────────────────────────────────

/// Upload options shared by tools.
pub struct FileTransferArgs<'a> {
    pub remote_path: &'a str,
    pub content: Option<&'a str>,
    pub encoding: Option<&'a str>,
    pub local_path: Option<&'a str>,
    pub create_dirs: bool,
    pub overwrite: bool,
    pub timeout_secs: Option<u64>,
    pub max_bytes: Option<u64>,
    pub async_mode: Option<bool>,
    pub verify: VerifyMode,
    pub cleanup_on_fail: bool,
    pub recursive: bool,
    pub prefer_rsync: Option<bool>,
}

pub async fn session_file_upload(
    rt: Arc<SessionRuntime>,
    cfg: &McpConfig,
    registry: &JobRegistry,
    args: FileTransferArgs<'_>,
) -> Result<Value, ToolError> {
    require_connected(&rt)?;
    check_session_allowed(&rt.id, cfg)?;

    let mut warnings = Vec::new();
    let cwd = tracked_cwd(&rt);
    if cwd.is_none() {
        warnings.push("cwd_unknown: relative remote_path is relative to remote $HOME".into());
    }
    let remote = resolve_remote_path(args.remote_path, cwd.as_deref())?;

    let has_content = args.content.is_some();
    let has_local = args
        .local_path
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();

    ops_log::log(
        "MCP",
        &format!(
            "transfer tool=session_file_upload sid={} mode={} recursive={} verify={} sandbox={} async={:?} content_len={} local={}",
            &rt.id[..rt.id.len().min(8)],
            if has_local { "local_path" } else { "content" },
            args.recursive,
            args.verify.as_str(),
            cfg.transfer_sandbox_enabled,
            args.async_mode,
            args.content.map(|c| c.len()).unwrap_or(0),
            args.local_path.unwrap_or("-")
        ),
    );

    if has_content && has_local {
        return Err(ToolError::invalid(
            "provide either content or local_path, not both",
        ));
    }
    if !has_content && !has_local {
        return Err(ToolError::invalid(
            "content or local_path is required for session_file_upload",
        ));
    }

    warnings.push(format!("verify={}", args.verify.as_str()));
    warnings.push(format!("cleanup_on_fail={}", args.cleanup_on_fail));
    warnings.push(format!("sandbox={}", cfg.transfer_sandbox_enabled));

    // ── local_path / scp|rsync ──────────────────────────────────────────────
    if has_local {
        let raw_local = PathBuf::from(args.local_path.unwrap().trim());
        let local = super::transfer_util::check_local_path(cfg, &raw_local, false)?;
        if args.recursive {
            if !local.is_dir() {
                return Err(ToolError::invalid(format!(
                    "recursive upload requires local directory: {}",
                    local.display()
                )));
            }
        } else if !local.is_file() {
            return Err(ToolError::file_not_found(format!(
                "local_path is not a file: {}",
                local.display()
            )));
        }
        let size = if args.recursive {
            dir_size_approx(&local)
        } else {
            local_file_size(&local)?
        };
        let timeout = Duration::from_secs(resolve_path_timeout_secs(args.timeout_secs, cfg));
        let want_async = args.async_mode.unwrap_or(true);
        let prefer_rsync = args.prefer_rsync.unwrap_or(cfg.transfer_prefer_rsync);
        let verify = if args.recursive && args.verify == VerifyMode::Sha256 {
            warnings.push("recursive: sha256 not supported; using none".into());
            VerifyMode::None
        } else {
            args.verify
        };

        if size > LARGE_FILE_HINT_BYTES {
            warnings.push(format!(
                "large_file: {size} bytes — path mode; content mode is capped at {HARD_MAX_TRANSFER_BYTES} bytes"
            ));
        }
        warnings.push(format!(
            "path_timeout_s={} max_retries={} prefer_rsync={}",
            timeout.as_secs(),
            cfg.transfer_max_retries,
            prefer_rsync
        ));

        if want_async {
            if registry.count_active() >= cfg.transfer_max_async_jobs as usize {
                return Err(ToolError::busy(format!(
                    "too many concurrent transfers (max {})",
                    cfg.transfer_max_async_jobs
                )));
            }
            let snap = spawn_path_job(
                registry,
                rt.clone(),
                SpawnOpts {
                    direction: TransferDirection::Upload,
                    local,
                    remote: remote.clone(),
                    bytes_total: Some(size),
                    create_dirs: args.create_dirs,
                    overwrite: args.overwrite,
                    verify,
                    cleanup_on_fail: args.cleanup_on_fail,
                    timeout,
                    warnings,
                    recursive: args.recursive,
                    prefer_rsync,
                    max_retries: cfg.transfer_max_retries,
                    retry_backoff: Duration::from_secs(cfg.transfer_retry_backoff_secs),
                    progress_poll_secs: cfg.transfer_progress_poll_secs,
                },
            )?;
            return Ok(json!({
                "async_transfer": true,
                "job_id": snap.job_id,
                "status": snap.status,
                "session_id": snap.session_id,
                "direction": snap.direction,
                "local_path": snap.local_path,
                "remote_path": snap.remote_path,
                "bytes_total": snap.bytes_total,
                "verify_mode": snap.verify_mode,
                "warnings": snap.warnings,
                "message": "transfer started; poll session_file_transfer_status (progress ~2s)",
            }));
        }

        begin_inflight(&rt)?;
        let t0 = Instant::now();
        let params = connect_params_from_rt(&rt)?;
        let plan = PathTransferOpts {
            direction: TransferDirection::Upload,
            local_final: local,
            remote_final: remote.clone(),
            job_id: Uuid::new_v4().to_string(),
            expected_bytes: Some(size),
            create_dirs: args.create_dirs,
            overwrite: args.overwrite,
            verify,
            cleanup_on_fail: args.cleanup_on_fail,
            timeout,
            progress_rt: None,
            recursive: args.recursive,
            prefer_rsync,
            max_retries: cfg.transfer_max_retries,
            retry_backoff: Duration::from_secs(cfg.transfer_retry_backoff_secs),
            progress_poll_secs: cfg.transfer_progress_poll_secs,
        };
        let outcome = execute_path_transfer(&rt, &params, &plan, None, None).await;
        end_inflight(&rt);
        let done = outcome.map_err(|(e, _)| e)?;
        warnings.extend(done.warnings);
        return Ok(serde_json::to_value(UploadResult {
            session_id: rt.id.clone(),
            remote_path: remote,
            bytes: done.bytes,
            mode: "local_path".into(),
            duration_ms: t0.elapsed().as_millis() as u64,
            warnings,
            verified: done.verified,
            verify_mode: done.verify_mode.as_str().into(),
            sha256: done.sha256,
            job_id: None,
            async_transfer: Some(false),
        })
        .unwrap_or_else(|e| json!({"error": e.to_string()})));
    }

    // ── content mode ────────────────────────────────────────────────────────
    if args.content.map(|c| c.len() as u64).unwrap_or(0) > HARD_MAX_TRANSFER_BYTES as u64 {
        return Err(ToolError::invalid(format!(
            "content mode max is {HARD_MAX_TRANSFER_BYTES} bytes; for large files use local_path (async scp)"
        )));
    }
    let enc = args.encoding.unwrap_or("utf8");
    let data = decode_content(args.content.unwrap_or(""), enc)?;
    let limit = resolve_max_transfer_bytes(args.max_bytes);
    if data.len() > limit {
        return Err(ToolError::invalid(format!(
            "content exceeds max_bytes ({limit}); use local_path mode for larger files"
        )));
    }
    if args.async_mode == Some(true) {
        return Err(ToolError::invalid(
            "async=true is only supported with local_path mode",
        ));
    }

    begin_inflight(&rt)?;
    let t0 = Instant::now();
    let timeout = Duration::from_secs(resolve_content_timeout_secs(args.timeout_secs, cfg));
    let params = connect_params_from_rt(&rt)?;
    let job_id = Uuid::new_v4().to_string();

    ops_log::log(
        "MCP",
        &format!(
            "tool=session_file_upload mode=content sid={} remote_len={} bytes={} verify={}",
            &rt.id[..rt.id.len().min(8)],
            remote.len(),
            data.len(),
            args.verify.as_str()
        ),
    );

    let done = execute_content_upload(
        &rt,
        &params,
        &remote,
        &data,
        args.create_dirs,
        args.overwrite,
        args.verify,
        args.cleanup_on_fail,
        timeout,
        &job_id,
    )
    .await;
    end_inflight(&rt);
    let done = done?;
    warnings.extend(done.warnings);

    Ok(serde_json::to_value(UploadResult {
        session_id: rt.id.clone(),
        remote_path: remote,
        bytes: done.bytes,
        mode: "content".into(),
        duration_ms: t0.elapsed().as_millis() as u64,
        warnings,
        verified: done.verified,
        verify_mode: done.verify_mode.as_str().into(),
        sha256: done.sha256,
        job_id: None,
        async_transfer: Some(false),
    })
    .unwrap_or_else(|e| json!({"error": e.to_string()})))
}

pub async fn session_file_download(
    rt: Arc<SessionRuntime>,
    cfg: &McpConfig,
    registry: &JobRegistry,
    args: FileTransferArgs<'_>,
) -> Result<Value, ToolError> {
    require_connected(&rt)?;
    check_session_allowed(&rt.id, cfg)?;

    let mut warnings = Vec::new();
    let cwd = tracked_cwd(&rt);
    if cwd.is_none() {
        warnings.push("cwd_unknown: relative remote_path is relative to remote $HOME".into());
    }
    let remote = resolve_remote_path(args.remote_path, cwd.as_deref())?;
    let local = args.local_path.map(str::trim).filter(|s| !s.is_empty());

    ops_log::log(
        "MCP",
        &format!(
            "transfer tool=session_file_download sid={} mode={} recursive={} verify={} sandbox={} async={:?} local={}",
            &rt.id[..rt.id.len().min(8)],
            if local.is_some() { "local_path" } else { "content" },
            args.recursive,
            args.verify.as_str(),
            cfg.transfer_sandbox_enabled,
            args.async_mode,
            local.unwrap_or("-")
        ),
    );

    warnings.push(format!("verify={}", args.verify.as_str()));
    warnings.push(format!("cleanup_on_fail={}", args.cleanup_on_fail));

    // ── local_path / scp|rsync ──────────────────────────────────────────────
    if let Some(lp) = local {
        let dest = super::transfer_util::check_local_path(cfg, Path::new(lp), true)?;
        let timeout = Duration::from_secs(resolve_path_timeout_secs(args.timeout_secs, cfg));
        let want_async = args.async_mode.unwrap_or(true);
        let prefer_rsync = args.prefer_rsync.unwrap_or(cfg.transfer_prefer_rsync);
        let verify = if args.recursive && args.verify == VerifyMode::Sha256 {
            warnings.push("recursive: sha256 not supported; using none".into());
            VerifyMode::None
        } else {
            args.verify
        };
        warnings.push(format!(
            "path_timeout_s={} max_retries={} prefer_rsync={} sandbox={}",
            timeout.as_secs(),
            cfg.transfer_max_retries,
            prefer_rsync,
            cfg.transfer_sandbox_enabled
        ));

        if want_async {
            if registry.count_active() >= cfg.transfer_max_async_jobs as usize {
                return Err(ToolError::busy(format!(
                    "too many concurrent transfers (max {})",
                    cfg.transfer_max_async_jobs
                )));
            }
            let snap = spawn_path_job(
                registry,
                rt.clone(),
                SpawnOpts {
                    direction: TransferDirection::Download,
                    local: dest,
                    remote: remote.clone(),
                    bytes_total: None,
                    create_dirs: false,
                    overwrite: args.overwrite,
                    verify,
                    cleanup_on_fail: args.cleanup_on_fail,
                    timeout,
                    warnings,
                    recursive: args.recursive,
                    prefer_rsync,
                    max_retries: cfg.transfer_max_retries,
                    retry_backoff: Duration::from_secs(cfg.transfer_retry_backoff_secs),
                    progress_poll_secs: cfg.transfer_progress_poll_secs,
                },
            )?;
            return Ok(json!({
                "async_transfer": true,
                "job_id": snap.job_id,
                "status": snap.status,
                "session_id": snap.session_id,
                "direction": snap.direction,
                "local_path": snap.local_path,
                "remote_path": snap.remote_path,
                "bytes_total": snap.bytes_total,
                "verify_mode": snap.verify_mode,
                "warnings": snap.warnings,
                "message": "transfer started; poll session_file_transfer_status",
            }));
        }

        begin_inflight(&rt)?;
        let t0 = Instant::now();
        let params = connect_params_from_rt(&rt)?;
        let plan = PathTransferOpts {
            direction: TransferDirection::Download,
            local_final: dest.clone(),
            remote_final: remote.clone(),
            job_id: Uuid::new_v4().to_string(),
            expected_bytes: None,
            create_dirs: false,
            overwrite: args.overwrite,
            verify,
            cleanup_on_fail: args.cleanup_on_fail,
            timeout,
            progress_rt: None,
            recursive: args.recursive,
            prefer_rsync,
            max_retries: cfg.transfer_max_retries,
            retry_backoff: Duration::from_secs(cfg.transfer_retry_backoff_secs),
            progress_poll_secs: cfg.transfer_progress_poll_secs,
        };
        let outcome = execute_path_transfer(&rt, &params, &plan, None, None).await;
        end_inflight(&rt);
        let done = outcome.map_err(|(e, _)| e)?;
        warnings.extend(done.warnings);
        return Ok(serde_json::to_value(DownloadResult {
            session_id: rt.id.clone(),
            remote_path: remote,
            bytes: done.bytes,
            mode: "local_path".into(),
            content: None,
            encoding: None,
            local_path: Some(dest.display().to_string()),
            truncated: false,
            duration_ms: t0.elapsed().as_millis() as u64,
            warnings,
            verified: done.verified,
            verify_mode: done.verify_mode.as_str().into(),
            sha256: done.sha256,
            job_id: None,
            async_transfer: Some(false),
        })
        .unwrap_or_else(|e| json!({"error": e.to_string()})));
    }

    // ── content mode ────────────────────────────────────────────────────────
    if args.async_mode == Some(true) {
        return Err(ToolError::invalid(
            "async=true requires local_path (content mode is always sync and size-capped)",
        ));
    }

    let limit = resolve_max_transfer_bytes(args.max_bytes);
    begin_inflight(&rt)?;
    let t0 = Instant::now();
    let timeout = Duration::from_secs(resolve_content_timeout_secs(args.timeout_secs, cfg));
    let params = connect_params_from_rt(&rt)?;

    // Optional size pre-check for verify.
    let mut verified = false;
    let mut sha256_out = None;
    let remote_sz = if args.verify != VerifyMode::None {
        match remote_file_size(&params, &rt, &remote, timeout.min(Duration::from_secs(60))).await {
            Ok(s) => Some(s),
            Err(e) => {
                end_inflight(&rt);
                return Err(e);
            }
        }
    } else {
        None
    };

    if let Some(sz) = remote_sz {
        if sz > limit as u64 && args.verify == VerifyMode::Size {
            warnings.push(format!(
                "remote size {sz} > max_bytes {limit}; content will truncate (use local_path for full file)"
            ));
        }
    }

    let q = shell_single_quote(&remote);
    let n = limit.saturating_add(1);
    let remote_cmd = format!("test -f {q} && test -r {q} && head -c {n} -- {q}");

    ops_log::log(
        "MCP",
        &format!(
            "tool=session_file_download mode=content sid={} remote_len={} max_bytes={}",
            &rt.id[..rt.id.len().min(8)],
            remote.len(),
            limit
        ),
    );

    let res = openssh::openssh_exec_bytes(
        &params,
        &remote_cmd,
        None,
        &rt.side_channel_key,
        control_path_of(&rt).as_deref(),
        timeout,
        limit + 1,
    )
    .await;
    end_inflight(&rt);
    let res = res.map_err(map_transfer_err)?;

    if res.exit_code.unwrap_or(1) != 0 {
        return Err(ToolError::file_not_found(format!(
            "cannot read remote file {remote}: {}",
            res.stderr.trim()
        )));
    }

    let mut data = res.stdout;
    let truncated = data.len() > limit || res.truncated;
    if data.len() > limit {
        data.truncate(limit);
        warnings.push(format!(
            "truncated: returned first {limit} bytes; for full large files use local_path + async scp"
        ));
    }

    match args.verify {
        VerifyMode::None => {}
        VerifyMode::Size => {
            if !truncated {
                if let Some(sz) = remote_sz {
                    if data.len() as u64 != sz {
                        return Err(ToolError::verify_failed(format!(
                            "size verify failed: content_len={} remote={sz}",
                            data.len()
                        )));
                    }
                    verified = true;
                }
            } else {
                warnings.push("size verify skipped: content truncated".into());
            }
        }
        VerifyMode::Sha256 => {
            if truncated {
                warnings.push("sha256 verify skipped: content truncated".into());
            } else {
                let local_h = sha256_bytes(&data);
                match remote_sha256(
                    &params,
                    &rt,
                    &remote,
                    timeout.min(Duration::from_secs(600)),
                )
                .await
                {
                    Ok(remote_h) if remote_h == local_h => {
                        verified = true;
                        sha256_out = Some(local_h);
                    }
                    Ok(remote_h) => {
                        return Err(ToolError::internal(format!(
                            "sha256 verify failed: local={local_h} remote={remote_h}"
                        )));
                    }
                    Err(e) => {
                        warnings.push(format!(
                            "sha256 verify unavailable ({}); falling back to size",
                            e.message
                        ));
                        if let Some(sz) = remote_sz {
                            if data.len() as u64 == sz {
                                verified = true;
                                sha256_out = Some(local_h);
                            } else {
                                return Err(ToolError::internal(format!(
                                    "size verify failed (sha256 fallback): content_len={} remote={sz}",
                                    data.len()
                                )));
                            }
                        }
                    }
                }
            }
        }
    }

    let prefer = args.encoding.unwrap_or("auto");
    let (text, enc_used) = encode_content(&data, prefer);

    Ok(serde_json::to_value(DownloadResult {
        session_id: rt.id.clone(),
        remote_path: remote,
        bytes: data.len() as u64,
        mode: "content".into(),
        content: Some(text),
        encoding: Some(enc_used),
        local_path: None,
        truncated,
        duration_ms: t0.elapsed().as_millis() as u64,
        warnings,
        verified,
        verify_mode: args.verify.as_str().into(),
        sha256: sha256_out,
        job_id: None,
        async_transfer: Some(false),
    })
    .unwrap_or_else(|e| json!({"error": e.to_string()})))
}

// ── job tools ───────────────────────────────────────────────────────────────

pub fn transfer_status(registry: &JobRegistry, job_id: &str) -> Result<Value, ToolError> {
    let job = registry
        .get(job_id.trim())
        .ok_or_else(|| ToolError::not_found(format!("transfer job not found: {job_id}")))?;
    Ok(serde_json::to_value(job.snapshot()).unwrap_or_else(|e| json!({"error": e.to_string()})))
}

pub fn transfer_cancel(registry: &JobRegistry, job_id: &str) -> Result<Value, ToolError> {
    let snap = registry.cancel(job_id.trim())?;
    ops_log::log(
        "MCP",
        &format!(
            "transfer cancel job={} status={:?} phase={:?} bytes={:?}/{:?}",
            &snap.job_id[..snap.job_id.len().min(8)],
            snap.status,
            snap.phase,
            snap.bytes_transferred,
            snap.bytes_total
        ),
    );
    Ok(json!({
        "cancelled": matches!(snap.status, TransferJobStatus::Cancelled)
            || snap.status == TransferJobStatus::Running
            || snap.status == TransferJobStatus::Queued,
        "job": snap,
        "message": "cancel requested; scp killed and staging cleaned when cleanup_on_fail=true",
    }))
}

pub fn transfer_list(
    registry: &JobRegistry,
    session_id: Option<&str>,
) -> Result<Value, ToolError> {
    let jobs = registry.list(session_id);
    Ok(json!({ "jobs": jobs, "count": jobs.len() }))
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_abs_and_rel() {
        assert_eq!(
            resolve_remote_path("/etc/hosts", Some("/home/u")).unwrap(),
            "/etc/hosts"
        );
        assert_eq!(
            resolve_remote_path("a/b", Some("/home/u")).unwrap(),
            "/home/u/a/b"
        );
    }

    #[test]
    fn staging_paths() {
        let r = remote_staging_path("/tmp/foo.bin", "abcd1234-xxxx");
        assert_eq!(r, "/tmp/foo.bin.anchorterm-part-abcd1234");
        let l = local_staging_path(Path::new(r"C:\data\out.bin"), "abcd1234-xxxx");
        assert!(l
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".anchorterm-part-abcd1234-"));
    }

    #[test]
    fn verify_parse() {
        assert_eq!(parse_verify_mode(None).unwrap(), VerifyMode::Size);
        assert_eq!(parse_verify_mode(Some("sha256")).unwrap(), VerifyMode::Sha256);
        assert_eq!(parse_verify_mode(Some("none")).unwrap(), VerifyMode::None);
        assert!(parse_verify_mode(Some("md5")).is_err());
    }

    #[test]
    fn sha256_bytes_hello() {
        // echo -n hello | sha256sum
        assert_eq!(
            sha256_bytes(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn path_timeout_clamped() {
        let cfg = McpConfig::default();
        assert_eq!(
            resolve_path_timeout_secs(None, &cfg),
            cfg.transfer_timeout_secs
        );
        assert_eq!(
            resolve_path_timeout_secs(Some(999_999), &cfg),
            cfg.transfer_max_timeout_secs
        );
    }

    #[test]
    fn job_registry_cancel_unknown() {
        let reg = JobRegistry::default();
        assert!(reg.cancel("nope").is_err());
    }
}
