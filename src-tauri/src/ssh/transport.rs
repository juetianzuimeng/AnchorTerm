//! SSH transport façade.
//!
//! Interactive sessions use the system OpenSSH client (`ssh -tt`) because russh
//! can accept `data_bytes` successfully while the remote shell still never
//! receives stdin (see production logs: write ok, no echo / no prompt change).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::AppHandle;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use crate::auth::AuthMethod;
use crate::error::AppError;
use crate::ssh::openssh;

/// Kept for legacy russh complete helpers (unused by interactive OpenSSH path).
pub(crate) struct ClientHandler;

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub cols: u32,
    pub rows: u32,
}

pub enum SessionCommand {
    Disconnect,
    Complete {
        line: String,
        cursor: usize,
        reply: tokio::sync::oneshot::Sender<Result<crate::ssh::complete::CompleteResult, String>>,
    },
    RemotePwd {
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    RemoteExec {
        command: String,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
}

/// Live interactive session.
pub struct ActiveTransport {
    pub cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    stdin: Arc<Mutex<ChildStdin>>,
    alive: Arc<AtomicBool>,
    /// Holds decrypted temp key material until the session ends (Drop wipes the file).
    _secure_key: Option<openssh::SecureKeyMaterial>,
}

impl ActiveTransport {
    /// Clone handles for writing without holding the transport mutex across await.
    pub fn clone_writer(&self) -> (Arc<Mutex<ChildStdin>>, Arc<AtomicBool>) {
        (Arc::clone(&self.stdin), Arc::clone(&self.alive))
    }
}

/// Write to a cloned OpenSSH stdin handle.
pub async fn write_stdin(
    stdin: &Arc<Mutex<ChildStdin>>,
    alive: &Arc<AtomicBool>,
    data: &[u8],
) -> Result<(), AppError> {
    if !alive.load(Ordering::SeqCst) {
        crate::ops_log::log("SSH", "write_stdin rejected: session not alive");
        return Err(AppError::NotConnected);
    }
    if data.is_empty() {
        return Ok(());
    }
    crate::ops_log::log(
        "SSH",
        &format!(
            "stdin write begin len={} hex={} text=\"{}\"",
            data.len(),
            crate::ops_log::hex_preview(data, 64),
            crate::ops_log::text_preview(data, 120)
        ),
    );
    let mut guard = stdin.lock().await;
    use tokio::io::AsyncWriteExt;
    if let Err(e) = guard.write_all(data).await {
        crate::ops_log::log("ERR", &format!("stdin write_all failed: {e}"));
        return Err(AppError::Ssh(format!("写入 SSH 失败: {e}")));
    }
    if let Err(e) = guard.flush().await {
        crate::ops_log::log("ERR", &format!("stdin flush failed: {e}"));
        return Err(AppError::Ssh(format!("刷新 SSH 失败: {e}")));
    }
    crate::ops_log::log("SSH", &format!("stdin write ok len={}", data.len()));
    Ok(())
}

/// Map low-level errors to readable Chinese messages (no secrets).
pub fn map_connect_error(e: impl ToString) -> AppError {
    let s = e.to_string();
    let lower = s.to_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        return AppError::Connect("连接超时，请检查主机地址、端口与网络".into());
    }
    if lower.contains("connection refused") || lower.contains("connection timed out") {
        return AppError::Connect("连接被拒绝或超时，请确认 SSH 服务与网络".into());
    }
    if lower.contains("permission denied") {
        return AppError::Auth("认证失败：用户名、密码或私钥不正确".into());
    }
    if lower.contains("no route") || lower.contains("network is unreachable") {
        return AppError::Connect("网络不可达，请检查网络连接".into());
    }
    if lower.contains("name or service not known")
        || lower.contains("could not resolve")
        || lower.contains("getaddrinfo")
    {
        return AppError::Connect("无法解析主机名，请检查主机地址".into());
    }
    AppError::Connect(s)
}

/// Connect interactive session via system OpenSSH.
pub async fn connect_session(
    app: AppHandle,
    params: ConnectParams,
) -> Result<ActiveTransport, AppError> {
    info!(
        host = %params.host,
        port = params.port,
        user = %params.username,
        "connect via OpenSSH"
    );

    let os = openssh::connect_openssh(app, params).await?;

    Ok(ActiveTransport {
        cmd_tx: os.cmd_tx,
        stdin: os.stdin,
        alive: os.alive,
        // Keep key file alive for the whole session (not only during auth).
        _secure_key: os.secure_key,
    })
}
