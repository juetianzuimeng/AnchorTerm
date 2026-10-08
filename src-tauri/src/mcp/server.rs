//! Localhost HTTP MCP endpoint (PR-M1 skeleton + M2–M4 tools).
//!
//! - Bearer token required
//! - `GET /health` — liveness
//! - `POST /mcp` — MCP JSON-RPC 2.0 (`initialize`, `tools/list`, `tools/call`, …)
//! - `POST /tools/call` — convenience REST for curl/scripts

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::AppHandle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::config::{is_loopback_host, McpConfig, PORT_FALLBACK_SPAN};
use super::tools::{self, tool_result_mcp_content};
use crate::error::AppError;
use crate::ops_log;

// Allow base64-encoded content uploads (~1–2 MiB payload + JSON overhead).
const MAX_BODY_BYTES: usize = 3 * 1024 * 1024;
const PHASE: &str = "PR-M5-stdio";

/// Running listener handle (stop via [`McpServerHandle::stop`]).
pub struct McpServerHandle {
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
    pub actual_port: u16,
    pub bind_host: String,
    running: Arc<AtomicBool>,
}

impl McpServerHandle {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst) && !self.join.is_finished()
    }

    pub async fn stop(self) {
        let _ = self.shutdown_tx.send(true);
        self.running.store(false, Ordering::SeqCst);
        let _ = tokio::time::timeout(Duration::from_millis(800), self.join).await;
        super::runtime_file::remove_runtime();
    }
}

/// Bind loopback, start accept loop. Port may bump from `cfg.port` on conflict.
pub async fn start_mcp_server(
    app: AppHandle,
    cfg: &McpConfig,
) -> Result<McpServerHandle, AppError> {
    if !cfg.enabled {
        return Err(AppError::Message("MCP 未启用".into()));
    }
    if !is_loopback_host(&cfg.bind_host) {
        return Err(AppError::Config(
            "MCP 仅允许绑定 127.0.0.1 / localhost / ::1".into(),
        ));
    }
    if cfg.token.trim().is_empty() {
        return Err(AppError::Config("MCP token 为空".into()));
    }

    let host = if cfg.bind_host.trim() == "localhost" {
        "127.0.0.1".to_string()
    } else {
        cfg.bind_host.trim().to_string()
    };

    let (listener, actual_port) = bind_with_fallback(&host, cfg.port).await?;
    let token = cfg.token.clone();
    let mcp_cfg = cfg.clone();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let running = Arc::new(AtomicBool::new(true));
    let running_flag = Arc::clone(&running);

    ops_log::log(
        "MCP",
        &format!("start bind={host}:{actual_port} (preferred={}) phase={PHASE}", cfg.port),
    );

    if let Err(e) = super::runtime_file::write_runtime(&host, actual_port) {
        ops_log::log("MCP", &format!("runtime file write failed: {e}"));
    }

    let join = tokio::spawn(async move {
        accept_loop(listener, app, token, mcp_cfg, shutdown_rx, running_flag).await;
        super::runtime_file::remove_runtime();
        ops_log::log("MCP", "accept loop exited");
    });

    Ok(McpServerHandle {
        shutdown_tx,
        join,
        actual_port,
        bind_host: host,
        running,
    })
}

async fn bind_with_fallback(host: &str, preferred: u16) -> Result<(TcpListener, u16), AppError> {
    let start = if preferred == 0 {
        super::config::DEFAULT_MCP_PORT
    } else {
        preferred
    };
    let end = start.saturating_add(PORT_FALLBACK_SPAN);
    let mut last_err = String::new();
    for port in start..=end {
        let addr = format!("{host}:{port}");
        match TcpListener::bind(&addr).await {
            Ok(l) => {
                let actual = l.local_addr().map(|a| a.port()).unwrap_or(port);
                return Ok((l, actual));
            }
            Err(e) => {
                last_err = format!("{addr}: {e}");
            }
        }
    }
    Err(AppError::Io(format!(
        "无法绑定 MCP 端口 {start}–{end}: {last_err}"
    )))
}

async fn accept_loop(
    listener: TcpListener,
    app: AppHandle,
    token: String,
    mcp_cfg: McpConfig,
    mut shutdown_rx: watch::Receiver<bool>,
    running: Arc<AtomicBool>,
) {
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((mut socket, peer)) => {
                        let token = token.clone();
                        let app = app.clone();
                        let mcp_cfg = mcp_cfg.clone();
                        tokio::spawn(async move {
                            ops_log::log("MCP", &format!("conn accepted peer={peer}"));
                            let mut buffer = Vec::new();
                            let mut req_count = 0;
                            loop {
                                match handle_connection(&mut socket, &mut buffer, &app, &token, &mcp_cfg, &peer).await {
                                    Ok(true) => {
                                        req_count += 1;
                                        continue;
                                    }
                                    Ok(false) => {
                                        ops_log::log(
                                            "MCP",
                                            &format!("conn closed peer={peer} reason=EOF requests={req_count}"),
                                        );
                                        break;
                                    }
                                    Err(e) => {
                                        ops_log::log(
                                            "MCP",
                                            &format!("conn closed peer={peer} reason='{e}' requests={req_count}"),
                                        );
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        ops_log::log("MCP", &format!("accept error: {e}"));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
    running.store(false, Ordering::SeqCst);
}

struct HttpRequest {
    method: String,
    path: String,
    headers: String,
    body: Vec<u8>,
}

async fn read_http_request(
    socket: &mut tokio::net::TcpStream,
    buf: &mut Vec<u8>,
) -> Result<Option<HttpRequest>, String> {
    let mut tmp = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    // Read until header end or cap.
    loop {
        if find_header_end(buf).is_some() {
            break;
        }

        if tokio::time::Instant::now() > deadline {
            return Err("header read timeout".into());
        }
        
        let read_timeout = if buf.is_empty() { Duration::from_secs(60) } else { Duration::from_secs(5) };
        let n_res = tokio::time::timeout(read_timeout, socket.read(&mut tmp)).await;
        
        let n = match n_res {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(format!("socket read error: {e}")),
            Err(_) => {
                if buf.is_empty() {
                    return Ok(None);
                } else {
                    return Err("read timeout (keep-alive/idle)".to_string());
                }
            }
        };

        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_BODY_BYTES + 8192 {
            return Err("request too large".into());
        }
        if buf.len() > 64 * 1024 && find_header_end(buf).is_none() {
            return Err("headers too large".into());
        }
    }

    if buf.is_empty() {
        return Ok(None);
    }

    let header_end = find_header_end(buf).ok_or_else(|| "incomplete HTTP headers".to_string())?;
    let header_bytes = &buf[..header_end];
    let headers = String::from_utf8_lossy(header_bytes).into_owned();
    let (method, path) = parse_request_line(&headers).unwrap_or(("GET".into(), "/".into()));

    let content_length = parse_content_length(&headers).unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err("body too large".into());
    }

    let total_required = header_end + content_length;
    while buf.len() < total_required {
        if tokio::time::Instant::now() > deadline {
            return Err("body read timeout".into());
        }
        let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut tmp))
            .await
            .map_err(|_| "body read timeout".to_string())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_BODY_BYTES + 8192 {
            return Err("body too large".into());
        }
    }

    if buf.len() < total_required {
        return Err("incomplete body".into());
    }

    let body = buf[header_end..total_required].to_vec();
    buf.drain(..total_required);

    Ok(Some(HttpRequest {
        method,
        path,
        headers,
        body,
    }))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
}

fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        if line.to_ascii_lowercase().starts_with("content-length:") {
            let v = line.split_once(':')?.1.trim();
            return v.parse().ok();
        }
    }
    None
}

async fn handle_connection(
    socket: &mut tokio::net::TcpStream,
    buffer: &mut Vec<u8>,
    app: &AppHandle,
    expected_token: &str,
    mcp_cfg: &McpConfig,
    peer: &std::net::SocketAddr,
) -> Result<bool, String> {
    let _ = socket.set_nodelay(true);
    let req = match read_http_request(socket, buffer).await? {
        Some(r) => r,
        None => return Ok(false),
    };
    let method = req.method.as_str();
    let path = req.path.as_str();

    ops_log::log(
        "MCP",
        &format!("req peer={peer} method={method} path={path} body_len={}", req.body.len()),
    );

    if !check_bearer(&req.headers, expected_token) {
        ops_log::log("MCP", &format!("deny bad_token peer={peer}"));
        write_http(
            socket,
            401,
            "application/json; charset=utf-8",
            r#"{"error":"unauthorized","message":"Bearer token required"}"#,
        )
        .await?;
        return Ok(true);
    }

    match (method, path) {
        ("GET", "/health") | ("GET", "/health/") => {
            let body = json!({
                "ok": true,
                "service": "anchorterm-mcp",
                "phase": PHASE,
                "tools": [
                    "sessions_list",
                    "session_get",
                    "session_exec",
                    "session_read_output",
                    "session_file_upload",
                    "session_file_download",
                    "session_file_transfer_status",
                    "session_file_transfer_cancel",
                    "session_file_transfer_pause",
                    "session_file_transfer_resume",
                    "session_file_transfer_list"
                ]
            });
            write_http(
                socket,
                200,
                "application/json; charset=utf-8",
                &body.to_string(),
            )
            .await?;
        }
        ("GET", "/") | ("GET", "/mcp") | ("GET", "/mcp/") => {
            let body = json!({
                "ok": true,
                "service": "anchorterm-mcp",
                "phase": PHASE,
                "message": "POST JSON-RPC to /mcp (initialize, tools/list, tools/call). REST: POST /tools/call",
            });
            write_http(
                socket,
                200,
                "application/json; charset=utf-8",
                &body.to_string(),
            )
            .await?;
        }
        ("GET", "/tools") | ("GET", "/tools/") => {
            let body = tools::tools_list_payload();
            write_http(
                socket,
                200,
                "application/json; charset=utf-8",
                &body.to_string(),
            )
            .await?;
        }
        ("POST", "/mcp") | ("POST", "/mcp/") => {
            let resp = handle_jsonrpc(app, mcp_cfg, &req.body, peer).await;
            write_http(
                socket,
                200,
                "application/json; charset=utf-8",
                &resp.to_string(),
            )
            .await?;
        }
        ("POST", "/tools/call") | ("POST", "/tools/call/") => {
            let resp = handle_rest_tools_call(app, mcp_cfg, &req.body, peer).await;
            let status = if resp.get("error").is_some() { 400 } else { 200 };
            write_http(
                socket,
                status,
                "application/json; charset=utf-8",
                &resp.to_string(),
            )
            .await?;
        }
        _ => {
            ops_log::log(
                "MCP",
                &format!("req 404 not found peer={peer} method={method} path={path}"),
            );
            write_http(
                socket,
                404,
                "application/json; charset=utf-8",
                r#"{"error":"not_found","message":"try GET /health, GET /tools, POST /mcp, POST /tools/call"}"#,
            )
            .await?;
        }
    }
    Ok(true)
}

async fn handle_jsonrpc(app: &AppHandle, cfg: &McpConfig, body: &[u8], peer: &std::net::SocketAddr) -> Value {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            ops_log::log("MCP", &format!("jsonrpc parse err peer={peer} err={e}"));
            return jsonrpc_error(Value::Null, -32700, format!("parse error: {e}"));
        }
    };

    // Batch not required for V1 — reject arrays politely.
    if parsed.is_array() {
        ops_log::log("MCP", &format!("jsonrpc batch reject peer={peer}"));
        return jsonrpc_error(Value::Null, -32600, "batch JSON-RPC not supported");
    }

    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let method = parsed
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let params = parsed.get("params").cloned().unwrap_or(json!({}));

    // Notifications (no id) — accept silently for initialized.
    let is_notification = parsed.get("id").is_none();

    match method {
        "initialize" => {
            let result = json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": { "listChanged": false }
                },
                "serverInfo": {
                    "name": "anchorterm",
                    "version": "0.1.0",
                    "phase": PHASE
                }
            });
            jsonrpc_result(id, result)
        }
        "notifications/initialized" | "initialized" => {
            if is_notification {
                // No response for pure notifications; return empty object for HTTP simplicity.
                return json!({});
            }
            jsonrpc_result(id, json!({}))
        }
        "ping" => jsonrpc_result(id, json!({})),
        "tools/list" => jsonrpc_result(id, tools::tools_list_payload()),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(json!({}));
            if name.is_empty() {
                ops_log::log("MCP", &format!("jsonrpc tools/call missing name peer={peer}"));
                return jsonrpc_error(id, -32602, "tools/call requires params.name");
            }
            match tools::call_tool(app, cfg, name, &arguments).await {
                Ok(val) => jsonrpc_result(id, tool_result_mcp_content(&val, false)),
                Err(e) => {
                    ops_log::log("MCP", &format!("jsonrpc tool err peer={peer} name={name} err={e:?}"));
                    // MCP: tool errors often returned as result with isError=true
                    let err_val = e.to_json();
                    jsonrpc_result(id, tool_result_mcp_content(&err_val, true))
                }
            }
        }
        "" => {
            ops_log::log("MCP", &format!("jsonrpc missing method peer={peer}"));
            jsonrpc_error(id, -32600, "missing method")
        }
        other => {
            ops_log::log("MCP", &format!("jsonrpc method not found peer={peer} method={other}"));
            jsonrpc_error(id, -32601, format!("method not found: {other}"))
        }
    }
}

async fn handle_rest_tools_call(app: &AppHandle, cfg: &McpConfig, body: &[u8], peer: &std::net::SocketAddr) -> Value {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            ops_log::log("MCP", &format!("rest tools parse err peer={peer} err={e}"));
            return json!({"error": "invalid_json", "message": e.to_string()});
        }
    };
    let name = parsed
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let arguments = parsed
        .get("arguments")
        .cloned()
        .unwrap_or(json!({}));
    if name.is_empty() {
        ops_log::log("MCP", &format!("rest tools missing name peer={peer}"));
        return json!({"error": "invalid_params", "message": "name is required"});
    }
    match tools::call_tool(app, cfg, name, &arguments).await {
        Ok(val) => json!({"ok": true, "result": val}),
        Err(e) => e.to_json(),
    }
}

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn jsonrpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into()
        }
    })
}

fn parse_request_line(req: &str) -> Option<(String, String)> {
    let line = req.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?;
    let path = path.split('?').next().unwrap_or(path).to_string();
    Some((method, path))
}

fn check_bearer(req: &str, expected: &str) -> bool {
    for line in req.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("authorization:") {
            let value = line.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
            if value.len() >= 7 && value[..7].eq_ignore_ascii_case("bearer ") {
                let got = value[7..].trim();
                return !expected.is_empty() && got == expected;
            }
        }
    }
    false
}

async fn write_http(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    // No CORS: this is a loopback control plane for local MCP clients, not browsers.
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: keep-alive\r\n\
         Keep-Alive: timeout=60\r\n\
         Cache-Control: no-store\r\n\
         \r\n",
        body.len()
    );
    socket
        .write_all(header.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    socket
        .write_all(body.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_check() {
        let req = "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer secret-token\r\n\r\n";
        assert!(check_bearer(req, "secret-token"));
        assert!(!check_bearer(req, "other"));
        assert!(!check_bearer("GET / HTTP/1.1\r\n\r\n", "secret-token"));
    }

    #[test]
    fn parse_path() {
        let req = "GET /health?x=1 HTTP/1.1\r\n\r\n";
        assert_eq!(
            parse_request_line(req).map(|(_, p)| p),
            Some("/health".into())
        );
    }

    #[test]
    fn content_length() {
        let h = "POST /mcp HTTP/1.1\r\nContent-Length: 42\r\n\r\n";
        assert_eq!(parse_content_length(h), Some(42));
    }
}
