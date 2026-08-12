//! PR-M5: stdio MCP bridge.
//!
//! Cursor / Claude Desktop spawn: `anchorterm.exe mcp-stdio`
//!
//! This process does **not** open the GUI. It reads JSON-RPC from stdin
//! (Content-Length framing and newline-delimited JSON) and forwards each
//! request to the running AnchorTerm HTTP MCP server (`POST /mcp`).
//!
//! Prerequisites: AnchorTerm GUI is running with MCP enabled.

use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{json, Value};

use super::config::{is_loopback_host, load_mcp_config, McpConfig, PORT_FALLBACK_SPAN};
use super::runtime_file;

/// Entry point for `anchorterm mcp-stdio` (blocking; no Tauri).
pub fn run_stdio_bridge() -> Result<(), String> {
    // Best-effort: attach parent console on Windows so `eprintln!` is visible
    // when launched from a terminal for debugging.
    #[cfg(windows)]
    attach_parent_console();

    let cfg = load_mcp_config().map_err(|e| {
        format!(
            "无法读取 MCP 配置 (%APPDATA%\\AnchorTerm\\mcp.json): {e}"
        )
    })?;

    if !cfg.enabled {
        return Err(
            "MCP 未启用。请先打开 AnchorTerm → 工具 → MCP 服务器 → 勾选启用并应用。"
                .into(),
        );
    }
    if cfg.token.trim().is_empty() {
        return Err("MCP token 为空，请在 AnchorTerm 中重新生成 Token。".into());
    }

    let (host, port) = resolve_endpoint(&cfg).map_err(|e| {
        format!(
            "{e}\n请确认：1) AnchorTerm 主程序正在运行 2) 已启用 MCP 3) 防火墙未拦截本机回环。"
        )
    })?;

    eprintln!(
        "anchorterm mcp-stdio: bridging stdio → http://{host}:{port}/mcp"
    );

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    loop {
        let msg = match read_message(&mut stdin) {
            Ok(Some(v)) => v,
            Ok(None) => break, // EOF
            Err(e) => {
                // Fatal framing error — report and exit.
                let err = jsonrpc_error(Value::Null, -32700, format!("stdio parse error: {e}"));
                let _ = write_message(&mut stdout, &err);
                return Err(e);
            }
        };

        // Notifications (no id): still forward; may get empty `{}` back.
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let is_notification = msg.get("id").is_none();

        match forward_jsonrpc(&host, port, &cfg.token, &msg) {
            Ok(resp) => {
                if is_notification {
                    // Drop empty server acks for notifications if client doesn't expect a body.
                    if resp.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                        continue;
                    }
                    // If server returned a full JSON-RPC object without id, skip.
                    if resp.get("id").is_none() && resp.get("result").is_none() && resp.get("error").is_none()
                    {
                        continue;
                    }
                }
                write_message(&mut stdout, &resp).map_err(|e| e.to_string())?;
            }
            Err(e) => {
                if is_notification {
                    eprintln!("anchorterm mcp-stdio: notification forward failed: {e}");
                    continue;
                }
                let err = jsonrpc_error(
                    id,
                    -32000,
                    format!("bridge to AnchorTerm failed: {e}"),
                );
                write_message(&mut stdout, &err).map_err(|e| e.to_string())?;
            }
        }
    }

    Ok(())
}

fn resolve_endpoint(cfg: &McpConfig) -> Result<(String, u16), String> {
    let mut candidates: Vec<(String, u16)> = Vec::new();

    if let Ok(Some(rt)) = runtime_file::load_runtime() {
        if is_loopback_host(&rt.bind_host) {
            candidates.push((normalize_host(&rt.bind_host), rt.port));
        }
    }

    let host = normalize_host(&cfg.bind_host);
    candidates.push((host.clone(), cfg.port));
    for p in 1..=PORT_FALLBACK_SPAN {
        candidates.push((host.clone(), cfg.port.saturating_add(p)));
    }

    // Dedup while preserving order.
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|(h, p)| seen.insert((h.clone(), *p)));

    let mut last_err = String::from("no candidate ports");
    // Prefer runtime file first (already ordered); use short connect timeout for probes.
    for (h, p) in candidates {
        match health_check(&h, p, &cfg.token) {
            Ok(()) => return Ok((h, p)),
            Err(e) => last_err = format!("{h}:{p}: {e}"),
        }
    }
    Err(format!(
        "无法连接 AnchorTerm MCP HTTP 服务（已尝试配置端口与 mcp.runtime.json）。最后错误: {last_err}"
    ))
}

fn normalize_host(h: &str) -> String {
    let t = h.trim();
    if t == "localhost" || t.is_empty() {
        "127.0.0.1".into()
    } else {
        t.to_string()
    }
}

fn health_check(host: &str, port: u16, token: &str) -> Result<(), String> {
    let path = "/health";
    let (status, body) = http_get(host, port, path, token, Duration::from_millis(250))?;
    if status != 200 {
        return Err(format!("HTTP {status}: {body}"));
    }
    if body.contains("unauthorized") {
        return Err("token rejected".into());
    }
    if !body.contains("anchorterm-mcp") {
        return Err("not an AnchorTerm MCP endpoint".into());
    }
    Ok(())
}

fn forward_jsonrpc(host: &str, port: u16, token: &str, msg: &Value) -> Result<Value, String> {
    let body = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    let (status, resp_body) =
        http_post_json(host, port, "/mcp", token, &body, Duration::from_secs(2))?;
    if status == 401 {
        return Err("unauthorized (token mismatch — regenerate in AnchorTerm?)".into());
    }
    if resp_body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&resp_body).map_err(|e| format!("invalid JSON response: {e}; body={resp_body}"))
}

fn http_get(
    host: &str,
    port: u16,
    path: &str,
    token: &str,
    connect_timeout: Duration,
) -> Result<(u16, String), String> {
    let mut stream = connect(host, port, connect_timeout)?;
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    read_http_response(&mut stream)
}

fn http_post_json(
    host: &str,
    port: u16,
    path: &str,
    token: &str,
    body: &[u8],
    connect_timeout: Duration,
) -> Result<(u16, String), String> {
    let mut stream = connect(host, port, connect_timeout)?;
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write headers: {e}"))?;
    stream
        .write_all(body)
        .map_err(|e| format!("write body: {e}"))?;
    read_http_response(&mut stream)
}

fn connect(host: &str, port: u16, connect_timeout: Duration) -> Result<TcpStream, String> {
    use std::net::{SocketAddr, ToSocketAddrs};
    let addr_str = format!("{host}:{port}");
    let addr: SocketAddr = addr_str
        .to_socket_addrs()
        .map_err(|e| format!("resolve {addr_str}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {addr_str}"))?;
    let stream = TcpStream::connect_timeout(&addr, connect_timeout)
        .map_err(|e| format!("connect {addr_str}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    Ok(stream)
}

fn read_http_response(stream: &mut TcpStream) -> Result<(u16, String), String> {
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let (header, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .ok_or_else(|| "invalid HTTP response (no header end)".to_string())?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    Ok((status, body.to_string()))
}

/// Read one MCP message: Content-Length framing **or** a single JSON line.
fn read_message<R: BufRead>(r: &mut R) -> Result<Option<Value>, String> {
    let mut first = String::new();
    let n = r.read_line(&mut first).map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed = first.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        // Skip blank lines.
        return read_message(r);
    }

    // Content-Length: N
    if trimmed.to_ascii_lowercase().starts_with("content-length:") {
        let mut content_length: Option<usize> = None;
        let mut line = first.clone();
        loop {
            let t = line.trim_end_matches(['\r', '\n']);
            if t.is_empty() {
                break;
            }
            let lower = t.to_ascii_lowercase();
            if lower.starts_with("content-length:") {
                let v = t.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
                content_length = Some(
                    v.parse()
                        .map_err(|_| format!("bad Content-Length: {v}"))?,
                );
            }
            line.clear();
            let n = r.read_line(&mut line).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("EOF while reading headers".into());
            }
        }
        let len = content_length.ok_or_else(|| "missing Content-Length".to_string())?;
        if len > 16 * 1024 * 1024 {
            return Err("message too large".into());
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        return Ok(Some(v));
    }

    // Newline-delimited JSON (single line object/array).
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let v: Value = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
        return Ok(Some(v));
    }

    Err(format!(
        "unrecognized stdio frame (expected Content-Length or JSON): {}",
        &trimmed[..trimmed.len().min(80)]
    ))
}

fn write_message<W: Write>(w: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    // Content-Length framing (widely supported by MCP clients).
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
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

#[cfg(windows)]
fn attach_parent_console() {
    // ATTACH_PARENT_PROCESS = -1. Ignore errors (no parent console is fine).
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parse_ndjson_line() {
        let data = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
        let mut c = Cursor::new(&data[..]);
        let v = read_message(&mut c).unwrap().unwrap();
        assert_eq!(v["method"], "ping");
    }

    #[test]
    fn parse_content_length() {
        let body = br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let mut raw = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        raw.extend_from_slice(body);
        let mut c = Cursor::new(raw);
        let v = read_message(&mut c).unwrap().unwrap();
        assert_eq!(v["id"], 2);
    }

    #[test]
    fn eof_returns_none() {
        let data = b"";
        let mut c = Cursor::new(&data[..]);
        assert!(read_message(&mut c).unwrap().is_none());
    }
}
