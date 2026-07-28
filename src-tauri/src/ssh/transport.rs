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

#[derive(Clone)]
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub cols: u32,
    pub rows: u32,
}

/// Control messages for the interactive OpenSSH session task.
pub enum SessionCommand {
    /// Close stdin and tear down the child process.
    Disconnect,
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

/// Connect interactive session via system OpenSSH.
///
/// `session_id` is captured by stdout pumps / `finish_session` so multi-session
/// can route events (client UUID, required).
pub async fn connect_session(
    app: AppHandle,
    params: ConnectParams,
    session_id: String,
) -> Result<ActiveTransport, AppError> {
    info!(
        host = %params.host,
        port = params.port,
        user = %params.username,
        session_id = %session_id,
        "connect via OpenSSH"
    );

    let os = openssh::connect_openssh(app, params, session_id).await?;

    Ok(ActiveTransport {
        cmd_tx: os.cmd_tx,
        stdin: os.stdin,
        alive: os.alive,
        // Keep key file alive for the whole session (not only during auth).
        _secure_key: os.secure_key,
    })
}
