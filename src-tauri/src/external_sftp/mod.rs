//! External SFTP launcher — open Xftp with a temporary `.xfp` session.
//!
//! Xftp auth quirks (validated against local Xftp 8 + Xshell sessions):
//! - `AuthMethodList` first code `00` = Password, `01` = Public Key
//! - Public-key sessions use `01,11,20,30` (not `01,20,30,10`)
//! - `UserKey` must be a **name in NetSarang UserKeys** (`*.pri`), never an
//!   OpenSSH filesystem path (paths cause Xftp to stay on Password UI)

mod netsarang_crypto;
mod xftp;

use std::path::PathBuf;

use serde::Serialize;
use tauri::State;

use crate::app_state::{AppState, SessionState};
use crate::auth::AuthMethod;
use crate::error::AppError;
use crate::ops_log;

pub use xftp::{detect_xftp_install, DetectXftpResult};

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum XftpAuthMode {
    /// Login password left empty — Xftp will prompt.
    PasswordPrompt,
    /// Account password embedded in temp `.xfp` (from session memory / keyring cache).
    PasswordEmbedded,
    /// Public key via NetSarang UserKeys name + optional passphrase from session.
    PublicKeyNamed,
    /// Public key requested but no matching UserKeys entry; Xftp may show Password.
    PublicKeyUnresolved,
}

#[derive(Debug, Serialize)]
pub struct LaunchXftpResponse {
    pub session_id: String,
    pub executable: String,
    pub xfp_path: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub remote: Option<String>,
    pub auth_mode: XftpAuthMode,
    /// NetSarang UserKey name when public-key mode.
    pub user_key_name: Option<String>,
    /// Human-readable Chinese hint for toast.
    pub user_hint: String,
    /// True when a second Xftp process may exit immediately (single-instance handoff).
    pub single_instance_ok: bool,
}

/// Detect Xftp installation (path + version hint).
#[tauri::command]
pub fn detect_xftp() -> DetectXftpResult {
    detect_xftp_install()
}

/// Open Xftp for a **connected** session using a temporary `.xfp` file.
#[tauri::command]
pub fn launch_xftp(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<LaunchXftpResponse, String> {
    launch_xftp_inner(&state, session_id.trim()).map_err(Into::into)
}

fn launch_xftp_inner(state: &AppState, session_id: &str) -> Result<LaunchXftpResponse, AppError> {
    if session_id.is_empty() {
        return Err(AppError::Message("session_id 不能为空".into()));
    }

    let rt = state.get_runtime(session_id)?;

    let (host, username, port, auth, remote) = {
        let meta = rt.meta.lock().expect("meta lock");
        if !matches!(meta.state, SessionState::Connected) {
            return Err(AppError::NotConnected);
        }
        let host = meta
            .host
            .clone()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| AppError::Message("当前会话缺少主机信息".into()))?;
        let username = meta
            .username
            .clone()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| AppError::Message("当前会话缺少用户名".into()))?;
        drop(meta);

        let cached = rt.cached.lock().expect("cached lock");
        let (port, auth) = if let Some(c) = cached.as_ref() {
            (c.port, Some(c.auth.clone()))
        } else {
            (22u16, None)
        };
        drop(cached);

        let remote = {
            let cwd = rt.cwd.lock().expect("cwd lock");
            cwd.last_known()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        };

        (host, username, port, auth, remote)
    };

    let detect = detect_xftp_install();
    let exe = detect
        .path
        .as_ref()
        .ok_or_else(|| {
            AppError::Message(
                "未找到 Xftp。请安装 Xftp 8，或确认安装路径包含 Xftp.exe。".into(),
            )
        })?
        .clone();

    let prepared = prepare_auth_for_xftp(auth.as_ref())?;

    // Xftp ignores plaintext secrets — encrypt with NetSarang session crypto (v7/v8).
    let login_password_enc = match prepared.login_password.as_deref() {
        Some(p) if !p.is_empty() => Some(netsarang_crypto::encrypt_session_secret(p)?),
        _ => None,
    };
    let user_key_pass_enc = match prepared.user_key_passphrase.as_deref() {
        Some(p) if !p.is_empty() => Some(netsarang_crypto::encrypt_session_secret(p)?),
        _ => None,
    };

    let xfp_path = xftp::write_temp_xfp(
        session_id,
        &host,
        port,
        &username,
        remote.as_deref(),
        prepared.user_key.as_deref(),
        login_password_enc.as_deref(),
        user_key_pass_enc.as_deref(),
        prepared.password_auth,
    )?;

    ops_log::log(
        "SFTP",
        &format!(
            "launch_xftp sid={} host={} port={} user={} remote={} exe={} xfp={} auth_mode={:?} user_key={} key_pass_enc={} login_pass_enc={} auth_list={}",
            &session_id[..session_id.len().min(8)],
            host,
            port,
            username,
            remote.as_deref().unwrap_or("-"),
            exe,
            xfp_path.display(),
            prepared.auth_mode,
            prepared.user_key.as_deref().unwrap_or("-"),
            user_key_pass_enc.is_some(),
            login_password_enc.is_some(),
            if prepared.password_auth {
                xftp::auth_method_list_password()
            } else {
                xftp::auth_method_list_public_key()
            }
        ),
    );

    xftp::spawn_xftp(&exe, &xfp_path)?;

    let mut cleanup = prepared.cleanup_paths;
    cleanup.push(xfp_path.clone());
    // Passphrase may sit in .xfp briefly — clean within a few minutes.
    xftp::schedule_paths_cleanup(cleanup, std::time::Duration::from_secs(180));

    Ok(LaunchXftpResponse {
        session_id: session_id.to_string(),
        executable: exe,
        xfp_path: xfp_path.display().to_string(),
        host,
        port,
        username,
        remote,
        auth_mode: prepared.auth_mode,
        user_key_name: prepared.user_key.clone(),
        user_hint: prepared.user_hint,
        single_instance_ok: true,
    })
}

struct PreparedAuth {
    password_auth: bool,
    login_password: Option<String>,
    /// NetSarang UserKeys **name** (not a filesystem path).
    user_key: Option<String>,
    user_key_passphrase: Option<String>,
    auth_mode: XftpAuthMode,
    user_hint: String,
    cleanup_paths: Vec<PathBuf>,
}

fn prepare_auth_for_xftp(auth: Option<&AuthMethod>) -> Result<PreparedAuth, AppError> {
    match auth {
        Some(AuthMethod::PublicKey {
            private_key_path,
            passphrase,
        }) if !private_key_path.is_empty() => {
            let pass = passphrase
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            let resolved = xftp::resolve_netsarang_user_key_name(private_key_path);
            // Fallback: use path basename even if not found in store (user may
            // have imported under that exact name after our scan).
            let key_name = resolved.clone().or_else(|| {
                std::path::Path::new(private_key_path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty() && !s.contains(['/', '\\']))
            });

            if let Some(name) = key_name {
                let found = resolved.is_some();
                let has_pass = pass.is_some();
                let hint = if found && has_pass {
                    format!(
                        "已选公钥「{name}」并写入加密口令；Xftp 应直接用公钥登录"
                    )
                } else if found {
                    format!(
                        "已选公钥「{name}」；若密钥仍加密，Xftp 可能要求输入一次口令"
                    )
                } else if has_pass {
                    format!(
                        "未在 Xftp 用户密钥库找到匹配项，已用文件名「{name}」尝试公钥；\
                         请在 Xshell/Xftp「工具→用户密钥」导入同名密钥"
                    )
                } else {
                    format!(
                        "未在 Xftp 用户密钥库找到匹配项，已用文件名「{name}」尝试公钥；\
                         请先把密钥导入 Xftp 用户密钥库"
                    )
                };
                ops_log::log(
                    "SFTP",
                    &format!(
                        "xftp public_key user_key_name={name} found_in_store={found} has_passphrase={has_pass} src_path_set=1"
                    ),
                );
                Ok(PreparedAuth {
                    password_auth: false,
                    login_password: None,
                    user_key: Some(name),
                    user_key_passphrase: pass,
                    auth_mode: if found {
                        XftpAuthMode::PublicKeyNamed
                    } else {
                        XftpAuthMode::PublicKeyUnresolved
                    },
                    user_hint: hint,
                    cleanup_paths: vec![],
                })
            } else {
                ops_log::log(
                    "SFTP",
                    "xftp public_key: cannot derive UserKey name from path",
                );
                Ok(PreparedAuth {
                    password_auth: false,
                    login_password: None,
                    user_key: None,
                    user_key_passphrase: pass,
                    auth_mode: XftpAuthMode::PublicKeyUnresolved,
                    user_hint: "无法解析 Xftp 用户密钥名；请在 Xftp 中手动选公钥，\
                         或把密钥导入「用户密钥」库后重试"
                        .into(),
                    cleanup_paths: vec![],
                })
            }
        }
        Some(AuthMethod::Password { password, .. }) => {
            let pwd = password
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if let Some(p) = pwd {
                Ok(PreparedAuth {
                    password_auth: true,
                    login_password: Some(p),
                    user_key: None,
                    user_key_passphrase: None,
                    auth_mode: XftpAuthMode::PasswordEmbedded,
                    user_hint: "已写入加密登录密码，认证方式：账号密码".into(),
                    cleanup_paths: vec![],
                })
            } else {
                Ok(PreparedAuth {
                    password_auth: true,
                    login_password: None,
                    user_key: None,
                    user_key_passphrase: None,
                    auth_mode: XftpAuthMode::PasswordPrompt,
                    user_hint: "认证方式：账号密码（请在 Xftp 中输入）".into(),
                    cleanup_paths: vec![],
                })
            }
        }
        _ => Ok(PreparedAuth {
            password_auth: true,
            login_password: None,
            user_key: None,
            user_key_passphrase: None,
            auth_mode: XftpAuthMode::PasswordPrompt,
            user_hint: "请在 Xftp 中完成认证".into(),
            cleanup_paths: vec![],
        }),
    }
}
