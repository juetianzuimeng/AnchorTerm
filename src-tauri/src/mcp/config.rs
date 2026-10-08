//! MCP server configuration (`%APPDATA%\AnchorTerm\mcp.json`).
//!
//! Defaults: disabled, loopback only, generated token on first write.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::config_dir;
use crate::error::AppError;

/// Default preferred listen port (may bump on conflict).
pub const DEFAULT_MCP_PORT: u16 = 39201;

/// Max ports to try after the preferred one (inclusive range length = this + 1).
pub const PORT_FALLBACK_SPAN: u16 = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// Master switch. Default false (safe).
    #[serde(default)]
    pub enabled: bool,
    /// Must remain loopback; non-loopback values are rewritten on load.
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    /// Preferred port; runtime may bind a higher port if busy.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Bearer token for HTTP Authorization. Generated if empty.
    #[serde(default)]
    pub token: String,
    /// V1.5: allow interactive PTY tools. PR-M1 ignores; kept for forward schema.
    #[serde(default)]
    pub allow_pty_tools: bool,
    /// Optional default session when tools omit session_id (tools in later PRs).
    #[serde(default)]
    pub default_session_id: Option<String>,
    #[serde(default = "default_exec_timeout")]
    pub exec_timeout_secs: u64,
    #[serde(default = "default_exec_max_output")]
    pub exec_max_output_bytes: usize,
    /// When `Some`, only these session_ids are visible/usable by MCP tools.
    #[serde(default)]
    pub allowed_session_ids: Option<Vec<String>>,

    // ── file transfer (MCP) ────────────────────────────────────────────────
    /// Default path/scp timeout seconds (default 3600).
    #[serde(default = "default_transfer_timeout")]
    pub transfer_timeout_secs: u64,
    /// Hard max path timeout (default 7200).
    #[serde(default = "default_transfer_max_timeout")]
    pub transfer_max_timeout_secs: u64,
    /// Staging size poll interval for async progress (default 2).
    #[serde(default = "default_transfer_progress_poll")]
    pub transfer_progress_poll_secs: u64,
    /// Default verify mode: none | size | sha256 (default size).
    #[serde(default = "default_transfer_verify")]
    pub transfer_default_verify: String,
    /// Auto-retries for retryable transfer errors (default 2).
    #[serde(default = "default_transfer_max_retries")]
    pub transfer_max_retries: u32,
    /// Backoff seconds between retries (default 3).
    #[serde(default = "default_transfer_retry_backoff")]
    pub transfer_retry_backoff_secs: u64,
    /// Restrict local_path to sandbox prefixes (default **false** — opt-in).
    #[serde(default)]
    pub transfer_sandbox_enabled: bool,
    /// Allowed local path prefixes; default = Downloads/AnchorTerm + LocalAppData/AnchorTerm/transfers.
    #[serde(default)]
    pub transfer_allow_local_prefixes: Option<Vec<String>>,
    /// Prefer rsync when available (partial resume); fall back to scp (default true).
    #[serde(default = "default_true")]
    pub transfer_prefer_rsync: bool,
    /// Soft max concurrent async transfer jobs (default 4).
    #[serde(default = "default_transfer_max_async")]
    pub transfer_max_async_jobs: u32,
}

fn default_bind_host() -> String {
    "127.0.0.1".into()
}

fn default_port() -> u16 {
    DEFAULT_MCP_PORT
}

fn default_exec_timeout() -> u64 {
    30
}

fn default_exec_max_output() -> usize {
    262_144
}

fn default_transfer_timeout() -> u64 {
    3600
}

fn default_transfer_max_timeout() -> u64 {
    7200
}

fn default_transfer_progress_poll() -> u64 {
    2
}

fn default_transfer_verify() -> String {
    "size".into()
}

fn default_transfer_max_retries() -> u32 {
    6
}

fn default_transfer_retry_backoff() -> u64 {
    3
}

fn default_true() -> bool {
    true
}

fn default_transfer_max_async() -> u32 {
    4
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_host: default_bind_host(),
            port: DEFAULT_MCP_PORT,
            token: String::new(),
            allow_pty_tools: false,
            default_session_id: None,
            exec_timeout_secs: default_exec_timeout(),
            exec_max_output_bytes: default_exec_max_output(),
            allowed_session_ids: None,
            transfer_timeout_secs: default_transfer_timeout(),
            transfer_max_timeout_secs: default_transfer_max_timeout(),
            transfer_progress_poll_secs: default_transfer_progress_poll(),
            transfer_default_verify: default_transfer_verify(),
            transfer_max_retries: default_transfer_max_retries(),
            transfer_retry_backoff_secs: default_transfer_retry_backoff(),
            transfer_sandbox_enabled: false,
            transfer_allow_local_prefixes: None,
            transfer_prefer_rsync: true,
            transfer_max_async_jobs: default_transfer_max_async(),
        }
    }
}

impl McpConfig {
    /// Ensure token exists and bind host is loopback-only.
    pub fn sanitize_inplace(&mut self) {
        if !is_loopback_host(&self.bind_host) {
            self.bind_host = default_bind_host();
        }
        if self.port == 0 {
            self.port = DEFAULT_MCP_PORT;
        }
        if self.token.trim().is_empty() {
            self.token = Uuid::new_v4().to_string();
        }
        if self.exec_timeout_secs == 0 {
            self.exec_timeout_secs = default_exec_timeout();
        }
        if self.exec_max_output_bytes == 0 {
            self.exec_max_output_bytes = default_exec_max_output();
        }
        if self.transfer_timeout_secs == 0 {
            self.transfer_timeout_secs = default_transfer_timeout();
        }
        if self.transfer_max_timeout_secs == 0 {
            self.transfer_max_timeout_secs = default_transfer_max_timeout();
        }
        if self.transfer_max_timeout_secs < self.transfer_timeout_secs {
            self.transfer_max_timeout_secs = self.transfer_timeout_secs;
        }
        if self.transfer_progress_poll_secs == 0 {
            self.transfer_progress_poll_secs = default_transfer_progress_poll();
        }
        if self.transfer_progress_poll_secs > 60 {
            self.transfer_progress_poll_secs = 60;
        }
        let v = self.transfer_default_verify.trim().to_ascii_lowercase();
        self.transfer_default_verify = match v.as_str() {
            "none" | "size" | "sha256" => v,
            _ => default_transfer_verify(),
        };
        if self.transfer_max_retries > 10 {
            self.transfer_max_retries = 10;
        }
        if self.transfer_retry_backoff_secs == 0 {
            self.transfer_retry_backoff_secs = default_transfer_retry_backoff();
        }
        if self.transfer_max_async_jobs == 0 {
            self.transfer_max_async_jobs = default_transfer_max_async();
        }
        if self.transfer_max_async_jobs > 16 {
            self.transfer_max_async_jobs = 16;
        }
    }
}

pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim();
    h == "127.0.0.1" || h == "localhost" || h == "::1"
}

pub fn mcp_config_path() -> Result<PathBuf, AppError> {
    Ok(config_dir()?.join("mcp.json"))
}

pub fn load_mcp_config() -> Result<McpConfig, AppError> {
    let path = mcp_config_path()?;
    if !path.exists() {
        let mut cfg = McpConfig::default();
        cfg.sanitize_inplace();
        return Ok(cfg);
    }
    let raw = fs::read_to_string(&path)?;
    let mut cfg: McpConfig = serde_json::from_str(&raw)?;
    cfg.sanitize_inplace();
    Ok(cfg)
}

pub fn save_mcp_config(cfg: &McpConfig) -> Result<(), AppError> {
    let mut owned = cfg.clone();
    owned.sanitize_inplace();
    let path = mcp_config_path()?;
    let raw = serde_json::to_string_pretty(&owned)?;
    fs::write(path, raw)?;
    Ok(())
}

/// Persist a default file if missing (generates token). Does not enable MCP.
pub fn ensure_mcp_config_file() -> Result<McpConfig, AppError> {
    let path = mcp_config_path()?;
    if path.exists() {
        return load_mcp_config();
    }
    let mut cfg = McpConfig::default();
    cfg.sanitize_inplace();
    save_mcp_config(&cfg)?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_public_bind() {
        let mut c = McpConfig {
            bind_host: "0.0.0.0".into(),
            token: String::new(),
            ..McpConfig::default()
        };
        c.sanitize_inplace();
        assert_eq!(c.bind_host, "127.0.0.1");
        assert!(!c.token.is_empty());
    }

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.1"));
    }
}
