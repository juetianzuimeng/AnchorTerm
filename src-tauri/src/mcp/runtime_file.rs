//! Runtime discovery file so `mcp-stdio` finds the live listen port.
//!
//! Path: `%APPDATA%\AnchorTerm\mcp.runtime.json`
//! Written when the HTTP MCP server starts; removed on stop.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::config_dir;
use crate::error::AppError;
use crate::ops_log;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRuntimeFile {
    pub bind_host: String,
    pub port: u16,
    pub pid: u32,
    /// ISO-ish timestamp for humans (not parsed).
    pub started_at: String,
}

pub fn runtime_path() -> Result<PathBuf, AppError> {
    Ok(config_dir()?.join("mcp.runtime.json"))
}

pub fn write_runtime(bind_host: &str, port: u16) -> Result<(), AppError> {
    let path = runtime_path()?;
    let info = McpRuntimeFile {
        bind_host: bind_host.to_string(),
        port,
        pid: std::process::id(),
        started_at: chrono_like_now(),
    };
    let raw = serde_json::to_string_pretty(&info)?;
    fs::write(&path, raw)?;
    ops_log::log(
        "MCP",
        &format!("runtime file written path={} port={}", path.display(), port),
    );
    Ok(())
}

pub fn remove_runtime() {
    if let Ok(path) = runtime_path() {
        if path.exists() {
            let _ = fs::remove_file(&path);
            ops_log::log("MCP", &format!("runtime file removed path={}", path.display()));
        }
    }
}

pub fn load_runtime() -> Result<Option<McpRuntimeFile>, AppError> {
    let path = runtime_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let info: McpRuntimeFile = serde_json::from_str(&raw)?;
    Ok(Some(info))
}

fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_shape() {
        let info = McpRuntimeFile {
            bind_host: "127.0.0.1".into(),
            port: 39201,
            pid: 1,
            started_at: "unix:0".into(),
        };
        let s = serde_json::to_string(&info).unwrap();
        let back: McpRuntimeFile = serde_json::from_str(&s).unwrap();
        assert_eq!(back.port, 39201);
    }
}
