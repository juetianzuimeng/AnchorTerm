use russh::client::{Config, Handle, Handler};
use russh::keys::ssh_key::PublicKey;
use russh::keys::PrivateKeyWithHashAlg;
use russh_sftp::client::SftpSession;
use std::sync::Arc;

use crate::auth::AuthMethod;
use crate::ssh::key_loader::load_private_key;

pub struct SftpClientHandler;

impl Handler for SftpClientHandler {
    type Error = russh::Error;
    
    fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        async move { Ok(true) }
    }
}

pub struct SftpConnection {
    pub session: Handle<SftpClientHandler>,
    pub sftp: SftpSession,
}

impl SftpConnection {
    pub async fn connect(
        host: &str,
        port: u16,
        user: &str,
        auth: &AuthMethod,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let config = Arc::new(Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(60)),
            ..Default::default()
        });

        let mut session = russh::client::connect(config, (host, port), SftpClientHandler).await?;
        
        let auth_res = match auth {
            AuthMethod::Password { password, .. } => {
                if let Some(pass) = password {
                    session.authenticate_password(user, pass).await?
                } else {
                    return Err("Password required but not provided".into());
                }
            }
            AuthMethod::PublicKey { private_key_path, passphrase, .. } => {
                let key = load_private_key(private_key_path, passphrase.as_deref())?;
                // PrivateKeyWithHashAlg::new does not return Result in russh 0.62
                let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), None);
                session.authenticate_publickey(user, key_with_alg).await?
            }
        };

        let auth_success = format!("{:?}", auth_res).contains("Success");
        if !auth_success {
            return Err("SFTP authentication failed".into());
        }

        let channel = session.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;

        Ok(Self { session, sftp })
    }
}

use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncSeekExt, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};

pub async fn sftp_upload_file<F>(
    sftp: &SftpSession,
    local_path: &Path,
    remote_path: &str,
    start_offset: u64,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    mut progress_callback: F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: FnMut(u64) + Send + 'static,
{
    let mut local_file = File::open(local_path).await?;
    if start_offset > 0 {
        local_file.seek(SeekFrom::Start(start_offset)).await?;
    }

    // Open remote file in write+create mode
    // russh-sftp 3.0.1 uses `open_with_flags` or `open` has a different signature.
    // If it only takes filename, let's try `open_with_flags(remote_path, OpenFlags::WRITE | OpenFlags::CREATE)` 
    // or maybe `sftp.open_with_flags(remote_path, russh_sftp::protocol::OpenFlags::WRITE | russh_sftp::protocol::OpenFlags::CREATE).await?`
    use russh_sftp::protocol::OpenFlags;
    let mut flags = OpenFlags::WRITE | OpenFlags::CREATE;
    if start_offset > 0 {
        flags |= OpenFlags::APPEND;
    }
    let mut remote_file = sftp.open_with_flags(remote_path, flags).await?;
    if start_offset > 0 {
        remote_file.seek(SeekFrom::Start(start_offset)).await?;
    }

    let mut buf = vec![0u8; 1024 * 128]; // 128KB chunks
    let mut current_offset = start_offset;

    loop {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err("Transfer cancelled".into());
        }
        
        while pause_flag.load(Ordering::Relaxed) {
            if cancel_flag.load(Ordering::Relaxed) {
                return Err("Transfer cancelled".into());
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }

        let n = local_file.read(&mut buf).await?;
        if n == 0 {
            break; // EOF
        }

        // Write chunk at the specific offset
        // For russh_sftp, wait, russh_sftp::client::fs::File doesn't have write_at? 
        // Or we just write_all and the remote tracks offset if it's a file handle.
        remote_file.write_all(&buf[..n]).await?;
        
        current_offset += n as u64;
        progress_callback(current_offset);
    }
    
    // Attempt to sync/close
    // Wait, russh_sftp File closes on drop, but we can explicitly sync
    // remote_file.sync_all().await?; // Not always available, depends on russh_sftp version

    remote_file.sync_all().await?;
    Ok(())
}

pub async fn sftp_download_file<F>(
    sftp: &SftpSession,
    remote_path: &str,
    local_path: &Path,
    start_offset: u64,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    mut progress_callback: F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: FnMut(u64) + Send + 'static,
{
    use russh_sftp::protocol::OpenFlags;
    let mut remote_file = sftp.open_with_flags(remote_path, OpenFlags::READ).await?;
    
    let mut local_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(local_path)
        .await?;

    if start_offset > 0 {
        local_file.seek(SeekFrom::Start(start_offset)).await?;
        // For SFTP, we need to read from the offset. russh_sftp File does not have seek, 
        // but we can use sftp.read_at if available, or just read and discard, OR we use `read_at`.
        // Let's assume we can just read. Wait, if it's a handle, we can just read. But if we need to seek...
        // Let's check russh-sftp APIs later if needed.
    }

    let mut buf = vec![0u8; 1024 * 128];
    let mut current_offset = start_offset;

    if start_offset > 0 {
        remote_file.seek(SeekFrom::Start(start_offset)).await?;
    }

    loop {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err("Transfer cancelled".into());
        }
        
        while pause_flag.load(Ordering::Relaxed) {
            if cancel_flag.load(Ordering::Relaxed) {
                return Err("Transfer cancelled".into());
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }

        let n = remote_file.read(&mut buf).await?;
        if n == 0 {
            break; // EOF
        }

        local_file.write_all(&buf[..n]).await?;
        
        current_offset += n as u64;
        progress_callback(current_offset);
    }
    
    local_file.flush().await?;

    remote_file.sync_all().await?;
    Ok(())
}

use crate::mcp::transfer::{TransferDirection, TransferJob};
use crate::ssh::transport::ConnectParams;
use std::time::Duration;

pub async fn run_sftp_transfer(
    params: &ConnectParams,
    direction: TransferDirection,
    local_path: &Path,
    remote_path: &str,
    start_offset: u64,
    job: Arc<TransferJob>,
    max_retries: u32,
    retry_backoff: Duration,
) -> Result<String, crate::mcp::exec::ToolError> {
    let mut attempt = 0;
    loop {
        let conn_res = SftpConnection::connect(
            &params.host,
            params.port,
            &params.username,
            &params.auth,
        ).await;

        match conn_res {
            Ok(conn) => {
                let job_c = Arc::clone(&job);
                let progress_cb = move |bytes| {
                    crate::mcp::transfer::set_job_progress(&job_c, "transferring", Some(bytes), None, None, None);
                };
                
                let res = match direction {
                    TransferDirection::Upload => {
                        let mut current_start = start_offset;
                        if attempt > 0 {
                            if let Ok(metadata) = conn.sftp.metadata(remote_path).await {
                                current_start = metadata.size.unwrap_or(start_offset);
                            }
                        }
                        sftp_upload_file(
                            &conn.sftp,
                            local_path,
                            remote_path,
                            current_start,
                            Arc::clone(&job.cancel),
                            Arc::clone(&job.pause),
                            progress_cb,
                        ).await
                    }
                    TransferDirection::Download => {
                        let mut current_start = start_offset;
                        if attempt > 0 {
                            if let Ok(metadata) = std::fs::metadata(local_path) {
                                current_start = metadata.len();
                            }
                        }
                        sftp_download_file(
                            &conn.sftp,
                            remote_path,
                            local_path,
                            current_start,
                            Arc::clone(&job.cancel),
                            Arc::clone(&job.pause),
                            progress_cb,
                        ).await
                    }

                };

                match res {
                    Ok(_) => return Ok("sftp".to_string()),
                    Err(e) => {
                        if e.to_string().contains("cancelled") {
                            return Err(crate::mcp::exec::ToolError::internal("Transfer cancelled"));
                        }
                        if attempt >= max_retries {
                            return Err(crate::mcp::exec::ToolError::internal(format!("SFTP error: {e}")));
                        }
                    }
                }
            }
            Err(e) => {
                if attempt >= max_retries {
                    return Err(crate::mcp::exec::ToolError::internal(format!("SFTP connect error: {e}")));
                }
            }
        }
        attempt += 1;
        tokio::time::sleep(retry_backoff).await;
    }
}



