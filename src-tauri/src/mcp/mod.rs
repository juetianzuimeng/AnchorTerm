//! MCP server integration (PR-M1 … M5 + file transfer async).
//!
//! - Config: `%APPDATA%\AnchorTerm\mcp.json`
//! - Runtime port: `%APPDATA%\AnchorTerm\mcp.runtime.json`
//! - Localhost HTTP + Bearer token gate
//! - Tools: sessions / exec / read_output / file upload·download /
//!   transfer status·cancel·list
//! - JSON-RPC at `POST /mcp`; REST at `POST /tools/call`
//! - Stdio bridge: `anchorterm.exe mcp-stdio` (PR-M5)
//!
//! Lock rule: never hold `std::sync::MutexGuard` across `.await`.

mod config;
mod exec;
mod runtime_file;
mod server;
mod stdio_bridge;
mod tools;
mod transfer;
mod transfer_util;
mod sftp;

pub use config::McpConfig;
pub use transfer::JobRegistry;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::ops_log;
use server::McpServerHandle;

const PHASE: &str = "PR-M5-stdio";

/// CLI entry: `anchorterm mcp-stdio` (no GUI).
pub fn run_mcp_stdio() -> Result<(), String> {
    stdio_bridge::run_stdio_bridge()
}

/// In-memory MCP runtime owned by [`AppState`].
pub struct McpRuntime {
    pub config: McpConfig,
    pub server: Option<McpServerHandle>,
    /// Last start/stop error for UI.
    pub last_error: Option<String>,
    /// Async path/scp transfer jobs (500MB+ friendly).
    pub transfer_jobs: JobRegistry,
}

impl Default for McpRuntime {
    fn default() -> Self {
        let config = config::load_mcp_config().unwrap_or_else(|e| {
            ops_log::log("MCP", &format!("load config failed: {e}; using defaults"));
            let mut c = McpConfig::default();
            c.sanitize_inplace();
            c
        });
        Self {
            config,
            server: None,
            last_error: None,
            transfer_jobs: JobRegistry::default(),
        }
    }
}

/// Snapshot returned to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
    pub enabled: bool,
    pub running: bool,
    pub bind_host: String,
    pub port: u16,
    pub actual_port: Option<u16>,
    pub token: String,
    pub endpoint_url: Option<String>,
    pub health_url: Option<String>,
    /// HTTP / Streamable-style Client snippet (Bearer header).
    pub client_config_json: String,
    /// stdio Client snippet (`command` + `args: ["mcp-stdio"]`).
    pub stdio_client_config_json: String,
    /// Absolute path of this executable (for stdio config).
    pub exe_path: Option<String>,
    pub last_error: Option<String>,
    pub allow_pty_tools: bool,
    pub phase: String,
}

fn current_exe_path() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn status_from_runtime(rt: &McpRuntime) -> McpStatus {
    let running = rt.server.as_ref().map(|s| s.is_running()).unwrap_or(false);
    let (actual_port, bind_host) = match rt.server.as_ref() {
        Some(s) if s.is_running() => (Some(s.actual_port), s.bind_host.clone()),
        _ => (None, rt.config.bind_host.clone()),
    };
    let port_for_url = actual_port.unwrap_or(rt.config.port);
    let endpoint_url = if running {
        Some(format!("http://{}:{}/mcp", bind_host, port_for_url))
    } else {
        None
    };
    let health_url = if running {
        Some(format!("http://{}:{}/health", bind_host, port_for_url))
    } else {
        None
    };
    let exe = current_exe_path();
    let client_config_json =
        build_http_client_config(&bind_host, port_for_url, &rt.config.token, running);
    let stdio_client_config_json =
        build_stdio_client_config(exe.as_deref(), running);
    McpStatus {
        enabled: rt.config.enabled,
        running,
        bind_host,
        port: rt.config.port,
        actual_port,
        token: rt.config.token.clone(),
        endpoint_url,
        health_url,
        client_config_json,
        stdio_client_config_json,
        exe_path: exe,
        last_error: rt.last_error.clone(),
        allow_pty_tools: rt.config.allow_pty_tools,
        phase: PHASE.into(),
    }
}

fn build_http_client_config(host: &str, port: u16, token: &str, running: bool) -> String {
    let url = format!("http://{host}:{port}/mcp");
    let note = if running {
        "HTTP JSON-RPC MCP. Prefer stdio (mcp-stdio) for Cursor/Claude Desktop if URL headers are awkward."
    } else {
        "Enable MCP in AnchorTerm (工具 → MCP 服务器) before connecting."
    };
    let value = serde_json::json!({
        "mcpServers": {
            "anchorterm": {
                "url": url,
                "headers": {
                    "Authorization": format!("Bearer {token}")
                },
                "_comment": note
            }
        }
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
}

fn build_stdio_client_config(exe: Option<&str>, running: bool) -> String {
    let command = exe.unwrap_or(r"C:\Path\To\anchorterm.exe");
    let note = if running {
        "Stdio bridge. AnchorTerm GUI must stay running with MCP enabled. Token is read from mcp.json automatically."
    } else {
        "Start AnchorTerm and enable MCP first. Then restart the MCP client."
    };
    let value = serde_json::json!({
        "mcpServers": {
            "anchorterm": {
                "command": command,
                "args": ["mcp-stdio"],
                "_comment": note
            }
        }
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
}

fn take_server(state: &AppState) -> Option<McpServerHandle> {
    state.mcp.lock().ok().and_then(|mut g| g.server.take())
}

async fn stop_taken(handle: Option<McpServerHandle>) {
    if let Some(handle) = handle {
        let port = handle.actual_port;
        handle.stop().await;
        ops_log::log("MCP", &format!("stop port={port}"));
    }
}

async fn start_with_config(
    app: &AppHandle,
    state: &AppState,
    cfg: &McpConfig,
) -> Result<(), AppError> {
    if !cfg.enabled {
        return Ok(());
    }
    match server::start_mcp_server(app.clone(), cfg).await {
        Ok(handle) => {
            if handle.actual_port != cfg.port {
                ops_log::log(
                    "MCP",
                    &format!(
                        "port fallback preferred={} actual={}",
                        cfg.port, handle.actual_port
                    ),
                );
            }
            if let Ok(mut g) = state.mcp.lock() {
                g.server = Some(handle);
                g.last_error = None;
            }
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            if let Ok(mut g) = state.mcp.lock() {
                g.last_error = Some(msg.clone());
            }
            ops_log::log("MCP", &format!("start failed: {msg}"));
            Err(e)
        }
    }
}

async fn restart_if_enabled(app: &AppHandle, state: &AppState) -> Result<(), AppError> {
    let old = take_server(state);
    stop_taken(old).await;
    let cfg = state
        .mcp
        .lock()
        .map(|g| g.config.clone())
        .map_err(|_| AppError::Message("MCP 状态锁损坏".into()))?;
    start_with_config(app, state, &cfg).await
}

/// Called on app startup.
pub async fn bootstrap(app: AppHandle) {
    let state = app.state::<AppState>();
    if let Err(e) = config::ensure_mcp_config_file() {
        ops_log::log("MCP", &format!("ensure config file: {e}"));
    }
    if let Ok(cfg) = config::load_mcp_config() {
        if let Ok(mut g) = state.mcp.lock() {
            g.config = cfg;
        }
    }
    let enabled = state
        .mcp
        .lock()
        .map(|g| g.config.enabled)
        .unwrap_or(false);
    if enabled {
        let cfg = state.mcp.lock().ok().map(|g| g.config.clone());
        if let Some(cfg) = cfg {
            let _ = start_with_config(&app, &state, &cfg).await;
        }
    } else {
        ops_log::log("MCP", "disabled (config.enabled=false)");
    }
}

/// App quit / teardown.
pub async fn shutdown(state: &AppState) {
    let old = take_server(state);
    stop_taken(old).await;
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn mcp_get_status(state: State<'_, AppState>) -> Result<McpStatus, String> {
    let guard = state.mcp.lock().map_err(|e| e.to_string())?;
    Ok(status_from_runtime(&guard))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSetEnabledRequest {
    pub enabled: bool,
}

#[tauri::command]
pub async fn mcp_set_enabled(
    app: AppHandle,
    state: State<'_, AppState>,
    req: McpSetEnabledRequest,
) -> Result<McpStatus, String> {
    {
        let mut guard = state.mcp.lock().map_err(|e| e.to_string())?;
        guard.config.enabled = req.enabled;
        guard.config.sanitize_inplace();
        config::save_mcp_config(&guard.config).map_err(|e| -> String { e.into() })?;
    }
    ops_log::log("MCP", &format!("set enabled={}", req.enabled));

    let old = take_server(&state);
    stop_taken(old).await;

    if req.enabled {
        let cfg = state
            .mcp
            .lock()
            .map(|g| g.config.clone())
            .map_err(|e| e.to_string())?;
        start_with_config(&app, &state, &cfg)
            .await
            .map_err(|e| -> String { e.into() })?;
    } else if let Ok(mut g) = state.mcp.lock() {
        g.last_error = None;
    }

    let guard = state.mcp.lock().map_err(|e| e.to_string())?;
    Ok(status_from_runtime(&guard))
}

#[tauri::command]
pub async fn mcp_regenerate_token(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<McpStatus, String> {
    {
        let mut guard = state.mcp.lock().map_err(|e| e.to_string())?;
        guard.config.token = uuid::Uuid::new_v4().to_string();
        guard.config.sanitize_inplace();
        config::save_mcp_config(&guard.config).map_err(|e| -> String { e.into() })?;
    }
    ops_log::log("MCP", "token regenerated");
    restart_if_enabled(&app, &state)
        .await
        .map_err(|e| -> String { e.into() })?;
    let guard = state.mcp.lock().map_err(|e| e.to_string())?;
    Ok(status_from_runtime(&guard))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpUpdateSettingsRequest {
    pub port: Option<u16>,
}

#[tauri::command]
pub async fn mcp_update_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    req: McpUpdateSettingsRequest,
) -> Result<McpStatus, String> {
    {
        let mut guard = state.mcp.lock().map_err(|e| e.to_string())?;
        if let Some(p) = req.port {
            if p == 0 {
                return Err(AppError::Message("端口无效".into()).into());
            }
            guard.config.port = p;
        }
        guard.config.sanitize_inplace();
        config::save_mcp_config(&guard.config).map_err(|e| -> String { e.into() })?;
        ops_log::log(
            "MCP",
            &format!("settings updated port={}", guard.config.port),
        );
    }
    restart_if_enabled(&app, &state)
        .await
        .map_err(|e| -> String { e.into() })?;
    let guard = state.mcp.lock().map_err(|e| e.to_string())?;
    Ok(status_from_runtime(&guard))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpApplyRequest {
    pub enabled: bool,
    pub port: Option<u16>,
}

/// Apply enable + port in one restart (UI “应用” button).
#[tauri::command]
pub async fn mcp_apply(
    app: AppHandle,
    state: State<'_, AppState>,
    req: McpApplyRequest,
) -> Result<McpStatus, String> {
    {
        let mut guard = state.mcp.lock().map_err(|e| e.to_string())?;
        if let Some(p) = req.port {
            if p == 0 {
                return Err(AppError::Message("端口无效".into()).into());
            }
            guard.config.port = p;
        }
        guard.config.enabled = req.enabled;
        guard.config.sanitize_inplace();
        config::save_mcp_config(&guard.config).map_err(|e| -> String { e.into() })?;
        ops_log::log(
            "MCP",
            &format!(
                "apply enabled={} port={}",
                guard.config.enabled, guard.config.port
            ),
        );
    }

    let old = take_server(&state);
    stop_taken(old).await;

    if req.enabled {
        let cfg = state
            .mcp
            .lock()
            .map(|g| g.config.clone())
            .map_err(|e| e.to_string())?;
        start_with_config(&app, &state, &cfg)
            .await
            .map_err(|e| -> String { e.into() })?;
    } else if let Ok(mut g) = state.mcp.lock() {
        g.last_error = None;
    }

    let guard = state.mcp.lock().map_err(|e| e.to_string())?;
    Ok(status_from_runtime(&guard))
}

#[tauri::command]
pub async fn mcp_ui_transfer_start(
    _app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    local_path: String,
    remote_path: String,
    direction: String, // "upload" or "download"
    resume_job_id: Option<String>,
    cleanup_on_fail: Option<bool>,
) -> Result<serde_json::Value, String> {
    let (rt, cfg, registry) = {
        let rt = state.get_runtime(&session_id)
            .map_err(|_| format!("session not found: {session_id}"))?;
        let st = state.mcp.lock().map_err(|e| e.to_string())?;
        (rt, st.config.clone(), st.transfer_jobs.clone())
    };

    let args = transfer::FileTransferArgs {
        remote_path: &remote_path,
        content: None,
        encoding: None,
        local_path: Some(&local_path),
        create_dirs: true,
        overwrite: true,
        timeout_secs: None,
        max_bytes: None,
        async_mode: Some(true),
        verify: transfer::VerifyMode::None,
        cleanup_on_fail: cleanup_on_fail.unwrap_or(true),
        recursive: false,
        prefer_rsync: None,
        resume_job_id: resume_job_id.as_deref(),
    };

    if direction == "upload" {
        transfer::session_file_upload(rt, &cfg, &registry, args)
            .await
            .map_err(|e| e.message.clone())
    } else {
        transfer::session_file_download(rt, &cfg, &registry, args)
            .await
            .map_err(|e| e.message.clone())
    }
}

#[tauri::command]
pub fn mcp_ui_transfer_list(
    state: State<'_, AppState>,
    session_id: Option<String>,
) -> Result<Vec<transfer::TransferJobSnapshot>, String> {
    let st = state.mcp.lock().map_err(|e| e.to_string())?;
    Ok(st.transfer_jobs.list(session_id.as_deref()))
}

#[tauri::command]
pub fn mcp_ui_transfer_cancel(
    state: State<'_, AppState>,
    job_id: String,
) -> Result<transfer::TransferJobSnapshot, String> {
    let st = state.mcp.lock().map_err(|e| e.to_string())?;
    st.transfer_jobs.cancel(&job_id).map_err(|e| e.message.clone())
}

#[tauri::command]
pub fn mcp_ui_transfer_clear_done(state: State<'_, AppState>) -> Result<(), String> {
    let st = state.mcp.lock().map_err(|e| e.to_string())?;
    st.transfer_jobs.clear_done();
    Ok(())
}
