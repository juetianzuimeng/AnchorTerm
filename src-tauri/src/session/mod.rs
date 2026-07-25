use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tracing::{debug, info, warn};

use crate::app_state::{AppState, CachedConnect, SessionState};
use crate::auth::credentials;
use crate::auth::{AuthMethod, AuthType};
use crate::config::{
    self,
    profile::{HostProfile, SaveProfileRequest},
};
use crate::cwd::{restore_cd_command, CwdTracker};
use crate::error::AppError;
use crate::ssh::transport::{connect_session, write_stdin, ConnectParams, SessionCommand};

const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30];

fn emit_state(app: &AppHandle, state: &AppState) {
    let _ = app.emit("session://state", state.snapshot());
}

fn set_state(app: &AppHandle, state: &AppState, next: SessionState, message: Option<String>) {
    {
        let mut meta = state.meta.lock().expect("meta lock");
        meta.state = next;
        meta.message = message;
    }
    emit_state(app, state);
}

#[derive(Debug, Deserialize)]
pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub cols: u32,
    pub rows: u32,
    pub profile_id: Option<String>,
}

#[tauri::command]
pub async fn connect(
    app: AppHandle,
    state: State<'_, AppState>,
    mut req: ConnectRequest,
) -> Result<(), String> {
    connect_inner(&app, &state, &mut req, false)
        .await
        .map_err(Into::into)
}

async fn connect_inner(
    app: &AppHandle,
    state: &AppState,
    req: &mut ConnectRequest,
    is_reconnect: bool,
) -> Result<(), AppError> {
    {
        let transport = state.transport.lock().expect("transport lock");
        if transport.is_some() {
            return Err(AppError::AlreadyConnected);
        }
    }

    resolve_password(req)?;

    state.set_term_size(req.cols, req.rows);

    {
        let mut meta = state.meta.lock().expect("meta lock");
        meta.host = Some(req.host.clone());
        meta.username = Some(req.username.clone());
        if !is_reconnect {
            meta.attempt = 0;
        }
    }

    // Whether this user-initiated connect should restore the previous cwd
    // (manual disconnect → connect again on the same host/user).
    let mut restore_on_user_connect = false;

    if !is_reconnect {
        // User-initiated connect: enable auto-reconnect for this session.
        state.auto_reconnect.store(true, Ordering::SeqCst);
        // Invalidate any old reconnect loops.
        state.reconnect_gen.fetch_add(1, Ordering::SeqCst);

        // Same host+user as last session + absolute restore path → keep and restore.
        // Different endpoint or no path → clear (fresh login home).
        let prev = state.cached.lock().ok().and_then(|c| c.clone()).or_else(|| {
            let meta = state.meta.lock().ok()?;
            Some(CachedConnect {
                host: meta.host.clone().unwrap_or_default(),
                port: 22,
                username: meta.username.clone().unwrap_or_default(),
                auth: req.auth.clone(),
                profile_id: None,
            })
        });
        let same_endpoint = prev
            .as_ref()
            .map(|p| {
                !p.host.is_empty()
                    && !p.username.is_empty()
                    && p.host == req.host
                    && p.username == req.username
            })
            .unwrap_or(false);

        let keep_path = if same_endpoint {
            state
                .restore_target
                .lock()
                .ok()
                .and_then(|r| r.clone())
                .or_else(|| {
                    state
                        .cwd
                        .lock()
                        .ok()
                        .and_then(|c| c.last_known().map(|s| s.to_string()))
                })
                .filter(|p| p.starts_with('/'))
        } else {
            None
        };

        if let Some(path) = keep_path {
            restore_on_user_connect = true;
            if let Ok(mut rt) = state.restore_target.lock() {
                *rt = Some(path.clone());
            }
            if let Ok(mut cwd) = state.cwd.lock() {
                cwd.set(path.clone());
            }
            crate::ops_log::log(
                "CWD",
                &format!("user reconnect will restore path={path} (same host/user)"),
            );
        } else {
            if let Ok(mut cwd) = state.cwd.lock() {
                cwd.clear();
            }
            if let Ok(mut rt) = state.restore_target.lock() {
                *rt = None;
            }
            crate::ops_log::log("CWD", "user connect: no restore target (fresh cwd)");
        }

        state.cwd_freeze.store(false, Ordering::SeqCst);
        set_state(
            app,
            state,
            SessionState::Connecting,
            Some("正在连接…".into()),
        );
    }

    // Cache credentials in memory for reconnect (never written to disk here).
    {
        let mut cached = state.cached.lock().expect("cached lock");
        *cached = Some(CachedConnect {
            host: req.host.clone(),
            port: req.port,
            username: req.username.clone(),
            auth: req.auth.clone(),
            profile_id: req.profile_id.clone(),
        });
    }

    let params = ConnectParams {
        host: req.host.clone(),
        port: req.port,
        username: req.username.clone(),
        auth: req.auth.clone(),
        cols: req.cols,
        rows: req.rows,
    };

    crate::ops_log::log(
        "SSH",
        &format!(
            "connect_inner begin reconnect={is_reconnect} host={} port={} user={} cols={} rows={}",
            req.host, req.port, req.username, req.cols, req.rows
        ),
    );

    match connect_session(app.clone(), params).await {
        Ok(transport) => {
            if let AuthMethod::Password {
                password: Some(pw),
                save_password: true,
            } = &req.auth
            {
                if let Some(pid) = &req.profile_id {
                    if let Err(e) = credentials::save_password(pid, pw) {
                        warn!("failed to save password to keyring: {e}");
                    } else if let Ok(Some(mut profile)) = config::get_profile(pid) {
                        profile.has_saved_password = true;
                        let _ = config::upsert_profile(profile);
                    }
                }
            }

            *state.transport.lock().expect("transport lock") = Some(transport);

            {
                let mut meta = state.meta.lock().expect("meta lock");
                meta.attempt = 0;
            }
            set_state(
                app,
                state,
                SessionState::Connected,
                Some(if is_reconnect {
                    "重连成功".into()
                } else if restore_on_user_connect {
                    "已连接，正在恢复工作目录…".into()
                } else {
                    "已连接".into()
                }),
            );
            crate::ops_log::log(
                "STATE",
                &format!(
                    "connected reconnect={is_reconnect} user_restore={restore_on_user_connect} host={} user={}",
                    req.host, req.username
                ),
            );

            // Restore playbook: silent cd after shell is ready.
            // - Auto-reconnect (unexpected drop)
            // - Manual disconnect → user connect again on same host/user
            // First connect to a host (or different user): seed login $HOME via exec pwd.
            if is_reconnect || restore_on_user_connect {
                crate::ops_log::log(
                    "SSH",
                    &format!(
                        "run_restore_playbook start auto_reconnect={is_reconnect} user_restore={restore_on_user_connect}"
                    ),
                );
                run_restore_playbook(app, state).await;
            } else {
                // Seed so relative `cd foo` can be resolved after first login.
                schedule_seed_login_pwd(app, 900);
            }
            Ok(())
        }
        Err(e) => {
            crate::ops_log::log("ERR", &format!("connect failed: {e}"));
            if !is_reconnect {
                set_state(app, state, SessionState::Failed, Some(e.to_string()));
            }
            Err(e)
        }
    }
}

fn resolve_password(req: &mut ConnectRequest) -> Result<(), AppError> {
    if let AuthMethod::Password { password, .. } = &mut req.auth {
        if password.as_ref().map(|p| p.is_empty()).unwrap_or(true) {
            if let Some(pid) = &req.profile_id {
                if let Some(stored) = credentials::load_password(pid)? {
                    *password = Some(stored);
                }
            }
        }
        if password.as_ref().map(|p| p.is_empty()).unwrap_or(true) {
            return Err(AppError::Auth(
                "未提供密码（且配置中无已保存密码）".into(),
            ));
        }
    }
    Ok(())
}

/// Normalize absolute path for equality (trailing slashes).
fn paths_equal(a: &str, b: &str) -> bool {
    fn trim_slash(s: &str) -> &str {
        if s.len() > 1 {
            s.trim_end_matches('/')
        } else {
            s
        }
    }
    trim_slash(a) == trim_slash(b)
}

fn session_still_connected(state: &AppState) -> bool {
    let meta = state.meta.lock().expect("meta lock");
    matches!(meta.state, SessionState::Connected)
}

async fn send_pty_bytes(state: &AppState, data: Vec<u8>) -> bool {
    let (stdin, alive) = {
        let g = state.transport.lock().expect("transport lock");
        match g.as_ref() {
            Some(t) => t.clone_writer(),
            None => return false,
        }
    };
    match write_stdin(&stdin, &alive, &data).await {
        Ok(()) => true,
        Err(e) => {
            warn!("pty write failed: {e}");
            false
        }
    }
}

/// After reconnect: wait for shell, silent `cd` to frozen path on the **interactive PTY**.
///
/// Strategy (no unconditional double-send):
/// 1. Send `cd` **once**
/// 2. Unfreeze OSC 7 briefly; if shell integration reports a *different* absolute
///    path, send **one** retry
/// 3. Without OSC 7, trust the single send (directory existence already checked)
///
/// Note: side-channel `exec pwd` starts a *new* process (always at login $HOME) and
/// cannot observe the interactive shell cwd — do not use it to "verify" restore.
async fn run_restore_playbook(app: &AppHandle, state: &AppState) {
    // Prefer path frozen at disconnect — live last_known may already be $HOME
    // from the new login shell's OSC 7.
    let path = state
        .restore_target
        .lock()
        .ok()
        .and_then(|r| r.clone())
        .or_else(|| {
            state
                .cwd
                .lock()
                .ok()
                .and_then(|c| c.last_known().map(|s| s.to_string()))
        });

    let Some(path) = path else {
        info!("restore playbook: no cwd to restore");
        return;
    };

    if !path.starts_with('/') {
        info!(path = %path, "skip restore cd for non-absolute path");
        return;
    }

    // Block OSC 7 home reports while we inject the first cd.
    state.cwd_freeze.store(true, Ordering::SeqCst);
    if let Ok(mut tracker) = state.cwd.lock() {
        tracker.set(path.clone());
    }
    let _ = app.emit("session://cwd", path.clone());

    // Give login shell time to finish .bashrc / print banner before injecting cd.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    if !session_still_connected(state) {
        state.cwd_freeze.store(false, Ordering::SeqCst);
        return;
    }

    // Existence check via side-channel exec is fine (filesystem, not interactive cwd).
    let dir_ok = query_dir_exists(state, &path).await.unwrap_or(true);
    if !dir_ok {
        warn!(path = %path, "restore target missing on remote");
        state.cwd_freeze.store(false, Ordering::SeqCst);
        if let Ok(mut rt) = state.restore_target.lock() {
            *rt = None;
        }
        set_state(
            app,
            state,
            SessionState::Connected,
            Some(format!("重连成功，但目录已不存在: {path}")),
        );
        schedule_seed_login_pwd(app, 400);
        return;
    }

    let cmd = restore_cd_command(&path);
    info!(path = %path, "restore playbook: silent cd (first send)");

    if !send_pty_bytes(state, cmd.clone().into_bytes()).await {
        state.cwd_freeze.store(false, Ordering::SeqCst);
        set_state(
            app,
            state,
            SessionState::Connected,
            Some(format!("重连成功，但无法发送恢复目录命令: {path}")),
        );
        return;
    }

    // Allow the line discipline to run `cd`.
    tokio::time::sleep(Duration::from_millis(550)).await;
    if !session_still_connected(state) {
        state.cwd_freeze.store(false, Ordering::SeqCst);
        return;
    }

    // Observe OSC 7 (if installed). Side-channel pwd cannot see interactive cwd.
    state.cwd_freeze.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(800)).await;
    if !session_still_connected(state) {
        return;
    }

    let observed = state
        .cwd
        .lock()
        .ok()
        .and_then(|c| c.last_known().map(|s| s.to_string()));

    let need_retry = match observed.as_deref() {
        Some(actual) if actual.starts_with('/') => !paths_equal(actual, &path),
        // No absolute OSC observation — do not spam a second cd.
        _ => false,
    };

    if need_retry {
        info!(
            path = %path,
            observed = ?observed,
            "restore: OSC cwd mismatch, one retry"
        );
        state.cwd_freeze.store(true, Ordering::SeqCst);
        if !send_pty_bytes(state, cmd.into_bytes()).await {
            state.cwd_freeze.store(false, Ordering::SeqCst);
            set_state(
                app,
                state,
                SessionState::Connected,
                Some(format!("重连成功，二次恢复目录失败: {path}")),
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(450)).await;
        state.cwd_freeze.store(false, Ordering::SeqCst);
    }

    // Bookkeeping: prefer target path for next disconnect restore.
    if let Ok(mut tracker) = state.cwd.lock() {
        tracker.set(path.clone());
    }
    if let Ok(mut rt) = state.restore_target.lock() {
        *rt = Some(path.clone());
    }
    let _ = app.emit("session://cwd", path.clone());
    set_state(
        app,
        state,
        SessionState::Connected,
        Some(format!("已恢复工作目录: {path}")),
    );
    info!(path = %path, retry = need_retry, "restore playbook done");
    state.cwd_freeze.store(false, Ordering::SeqCst);
}

fn connect_params_from_cache(state: &AppState) -> Result<ConnectParams, String> {
    let cached = state
        .cached
        .lock()
        .expect("cached lock")
        .clone()
        .ok_or_else(|| "无会话凭据缓存".to_string())?;
    let (cols, rows) = state.term_size();
    Ok(ConnectParams {
        host: cached.host,
        port: cached.port,
        username: cached.username,
        auth: cached.auth,
        cols,
        rows,
    })
}

async fn query_remote_pwd(state: &AppState) -> Result<String, String> {
    let params = connect_params_from_cache(state)?;
    let out = crate::ssh::openssh::openssh_exec(&params, "pwd -P")
        .await
        .map_err(|e| e.to_string())?;
    let path = out.lines().next().unwrap_or("").trim().to_string();
    if path.starts_with('/') {
        Ok(path)
    } else {
        Err(format!("unexpected pwd output: {out:?}"))
    }
}

async fn query_dir_exists(state: &AppState, path: &str) -> Result<bool, String> {
    let params = connect_params_from_cache(state)?;
    let quoted = crate::cwd::shell_single_quote(path);
    let cmd = format!("test -d {quoted} && echo AT_DIR_OK || echo AT_DIR_MISSING");
    let out = crate::ssh::openssh::openssh_exec(&params, &cmd)
        .await
        .map_err(|e| e.to_string())?;
    Ok(out.contains("AT_DIR_OK"))
}

/// Seed last_known from a fresh login shell's pwd (initial home). Safe only when
/// the interactive shell has not yet changed directory.
fn schedule_seed_login_pwd(app: &AppHandle, delay_ms: u64) {
    let app = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let Some(state) = app.try_state::<AppState>() else {
            return;
        };
        if state.cwd_freeze.load(Ordering::SeqCst) {
            return;
        }
        {
            let meta = state.meta.lock().expect("meta lock");
            if !matches!(meta.state, SessionState::Connected) {
                return;
            }
        }
        // Only seed if we still have no absolute path.
        {
            let cwd = state.cwd.lock().expect("cwd lock");
            if cwd
                .last_known()
                .map(|p| p.starts_with('/'))
                .unwrap_or(false)
            {
                return;
            }
        }
        match query_remote_pwd(&state).await {
            Ok(path) => {
                if let Ok(mut tracker) = state.cwd.lock() {
                    tracker.set(path.clone());
                }
                if let Ok(mut rt) = state.restore_target.lock() {
                    *rt = Some(path.clone());
                }
                let _ = app.emit("session://cwd", path);
                let _ = app.emit("session://state", state.snapshot());
                debug!("seeded login pwd");
            }
            Err(e) => debug!(error = %e, "seed login pwd failed"),
        }
    });
}

/// Spawn background reconnect with exponential backoff.
pub fn spawn_reconnect_loop(app: AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if !state.auto_reconnect.load(Ordering::SeqCst) {
        return;
    }

    let gen = state.reconnect_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let app2 = app.clone();

    tokio::spawn(async move {
        reconnect_loop(app2, gen).await;
    });
}

async fn reconnect_loop(app: AppHandle, gen: u64) {
    let mut attempt: u32 = 0;

    loop {
        let Some(state) = app.try_state::<AppState>() else {
            return;
        };

        if state.reconnect_gen.load(Ordering::SeqCst) != gen {
            return; // cancelled
        }
        if !state.auto_reconnect.load(Ordering::SeqCst) {
            return;
        }
        // Already connected by someone else.
        if state.transport.lock().expect("t").is_some() {
            return;
        }

        attempt = attempt.saturating_add(1);
        let delay = BACKOFF_SECS
            [(attempt as usize - 1).min(BACKOFF_SECS.len() - 1)];

        {
            let mut meta = state.meta.lock().expect("meta lock");
            meta.state = SessionState::Reconnecting;
            meta.attempt = attempt;
            meta.message = Some(format!("重连中 ({attempt})，{delay}s 后重试…"));
        }
        emit_state(&app, &state);

        tokio::time::sleep(Duration::from_secs(delay)).await;

        if state.reconnect_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        if !state.auto_reconnect.load(Ordering::SeqCst) {
            return;
        }
        if state.transport.lock().expect("t").is_some() {
            return;
        }

        let cached = state.cached.lock().expect("cached").clone();
        let Some(cached) = cached else {
            set_state(
                &app,
                &state,
                SessionState::Failed,
                Some("无法自动重连：会话凭据已丢失，请重新连接".into()),
            );
            return;
        };

        let (cols, rows) = state.term_size();
        let mut req = ConnectRequest {
            host: cached.host,
            port: cached.port,
            username: cached.username,
            auth: cached.auth,
            cols,
            rows,
            profile_id: cached.profile_id,
        };

        {
            let mut meta = state.meta.lock().expect("meta lock");
            meta.message = Some(format!("重连中 ({attempt})…"));
        }
        emit_state(&app, &state);

        match connect_inner(&app, &state, &mut req, true).await {
            Ok(()) => {
                info!(attempt, "auto-reconnect succeeded");
                crate::ops_log::log("SSH", &format!("auto-reconnect succeeded attempt={attempt}"));
                return;
            }
            Err(e) => {
                warn!(attempt, error = %e, "auto-reconnect attempt failed");
                crate::ops_log::log(
                    "ERR",
                    &format!("auto-reconnect failed attempt={attempt} err={e}"),
                );
                // Permanent auth failures must not spin forever.
                let permanent = matches!(e, AppError::Auth(_));
                if permanent {
                    state.auto_reconnect.store(false, Ordering::SeqCst);
                    set_state(
                        &app,
                        &state,
                        SessionState::Failed,
                        Some(format!("认证失败，已停止自动重连: {e}")),
                    );
                    return;
                }
                {
                    let mut meta = state.meta.lock().expect("meta lock");
                    meta.state = SessionState::Reconnecting;
                    meta.attempt = attempt;
                    meta.message = Some(format!("重连失败 ({attempt}): {e}"));
                }
                emit_state(&app, &state);
            }
        }
    }
}

#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    disconnect_inner(&state, &app).map_err(Into::into)
}

fn disconnect_inner(state: &AppState, app: &AppHandle) -> Result<(), AppError> {
    crate::ops_log::log("UI", "disconnect manual");
    // Disable auto-reconnect for this session (manual disconnect).
    state.auto_reconnect.store(false, Ordering::SeqCst);
    state.reconnect_gen.fetch_add(1, Ordering::SeqCst);

    // Freeze absolute cwd so the next user-initiated connect can restore it.
    freeze_restore_target_from_cwd(state, "manual disconnect");

    let transport = state.transport.lock().expect("transport lock").take();
    if let Some(t) = transport {
        let _ = t.cmd_tx.send(SessionCommand::Disconnect);
    }

    {
        let mut meta = state.meta.lock().expect("meta lock");
        meta.state = SessionState::Idle;
        meta.message = Some("已手动断开".into());
        meta.attempt = 0;
    }
    emit_state(app, state);
    crate::ops_log::log("STATE", "idle (manual disconnect)");
    Ok(())
}

/// Remember absolute cwd for a later reconnect / user re-connect restore.
fn freeze_restore_target_from_cwd(state: &AppState, reason: &str) {
    let path = state
        .cwd
        .lock()
        .ok()
        .and_then(|c| c.last_known().map(|s| s.to_string()))
        .filter(|p| p.starts_with('/'));
    if let Some(path) = path {
        if let Ok(mut rt) = state.restore_target.lock() {
            *rt = Some(path.clone());
        }
        crate::ops_log::log(
            "CWD",
            &format!("freeze restore_target path={path} reason={reason}"),
        );
    } else {
        crate::ops_log::log(
            "CWD",
            &format!("freeze restore_target skipped (no absolute cwd) reason={reason}"),
        );
    }
}

#[tauri::command]
pub async fn write_bytes(state: State<'_, AppState>, data_b64: String) -> Result<(), String> {
    write_inner(&state, data_b64).await.map_err(Into::into)
}

async fn write_inner(state: &AppState, data_b64: String) -> Result<(), AppError> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| AppError::Message(format!("无效的 base64 输入: {e}")))?;

    // Track cd from submitted lines (fallback when no OSC 7).
    if let Ok(text) = std::str::from_utf8(&bytes) {
        for line in text.split(['\n', '\r']) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(mut cwd) = state.cwd.lock() {
                if let Some(path) = cwd.feed_submitted_line(line) {
                    if path.starts_with('/') {
                        if let Ok(mut rt) = state.restore_target.lock() {
                            *rt = Some(path);
                        }
                    }
                }
            }
        }
    }

    let (stdin, alive) = {
        let transport = state.transport.lock().expect("transport lock");
        transport
            .as_ref()
            .ok_or(AppError::NotConnected)?
            .clone_writer()
    };
    write_stdin(&stdin, &alive, &bytes).await?;
    Ok(())
}

/// Submit a full draft line (appends CR). Used by draft input box.
#[tauri::command]
pub async fn submit_line(
    state: State<'_, AppState>,
    app: AppHandle,
    line: String,
) -> Result<(), String> {
    submit_line_inner(&state, &app, line)
        .await
        .map_err(Into::into)
}

async fn submit_line_inner(
    state: &AppState,
    app: &AppHandle,
    line: String,
) -> Result<(), AppError> {
    // Draft is one logical shell line.
    let logical = line.trim_end_matches(['\n', '\r']);
    if logical.is_empty() {
        return Ok(());
    }

    let (stdin, alive) = {
        let transport = state.transport.lock().expect("transport lock");
        transport
            .as_ref()
            .ok_or(AppError::NotConnected)?
            .clone_writer()
    };

    // OpenSSH -tt PTY: type the line then CR (Enter). Send as one packet first;
    // character-by-character was not needed for stty, but we flush hard so the
    // Windows pipe → ssh bridge does not hold the line incomplete.
    let mut payload = logical.as_bytes().to_vec();
    payload.push(b'\r');

    info!(len = payload.len(), line = %logical, "submit_line → openssh stdin");
    crate::ops_log::log(
        "CMD",
        &format!(
            "submit_line line=\"{}\" payload_len={} hex={}",
            logical,
            payload.len(),
            crate::ops_log::hex_preview(&payload, 64)
        ),
    );
    write_stdin(&stdin, &alive, &payload).await?;
    info!("submit_line write ok");
    crate::ops_log::log("CMD", &format!("submit_line ok line=\"{logical}\""));

    // Track cwd from the submitted line (best-effort; OSC 7 remains source of truth).
    // IMPORTANT: release `cwd` before emit_state/snapshot — snapshot() also locks cwd
    // and std::sync::Mutex is not reentrant (deadlock froze the stdout pump after `cd`).
    let new_cwd = {
        let mut cwd = state.cwd.lock().expect("cwd lock");
        cwd.feed_submitted_line(logical)
    };
    if let Some(path) = new_cwd {
        crate::ops_log::log("CWD", &format!("from_cd_parse path={path}"));
        if path.starts_with('/') {
            if let Ok(mut rt) = state.restore_target.lock() {
                *rt = Some(path.clone());
            }
        }
        let _ = app.emit("session://cwd", &path);
        emit_state(app, state);
    }

    Ok(())
}

/// Tab completion for the draft input (remote compgen via side-channel OpenSSH exec).
#[tauri::command]
pub async fn complete_draft(
    state: State<'_, AppState>,
    line: String,
    cursor: usize,
) -> Result<crate::ssh::complete::CompleteResult, String> {
    complete_draft_inner(&state, line, cursor).await
}

async fn complete_draft_inner(
    state: &AppState,
    line: String,
    cursor: usize,
) -> Result<crate::ssh::complete::CompleteResult, String> {
    {
        let meta = state.meta.lock().expect("meta lock");
        if !matches!(meta.state, SessionState::Connected) {
            return Err("未连接，无法补全".into());
        }
    }

    let cwd = state
        .cwd
        .lock()
        .ok()
        .and_then(|c| c.last_known().map(|s| s.to_string()));

    let params = connect_params_from_cache(state)?;

    crate::ops_log::log(
        "CMD",
        &format!(
            "complete_draft cursor={cursor} cwd={} line=\"{}\"",
            cwd.as_deref().unwrap_or("."),
            crate::ops_log::text_preview(line.as_bytes(), 80)
        ),
    );

    let result = crate::ssh::complete::remote_complete_with_exec(
        cwd,
        line,
        cursor,
        |command| async move {
            crate::ops_log::log(
                "SSH",
                &format!(
                    "complete side-channel exec cmd_len={} preview=\"{}\"",
                    command.len(),
                    crate::ops_log::text_preview(command.as_bytes(), 100)
                ),
            );
            crate::ssh::openssh::openssh_exec(&params, &command)
                .await
                .map_err(|e| e.to_string())
        },
    )
    .await;

    match &result {
        Ok(r) => {
            crate::ops_log::log(
                "CMD",
                &format!(
                    "complete_draft ok candidates={} line=\"{}\"",
                    r.candidates.len(),
                    crate::ops_log::text_preview(r.line.as_bytes(), 80)
                ),
            );
        }
        Err(e) => {
            crate::ops_log::log("ERR", &format!("complete_draft failed: {e}"));
        }
    }

    result
}

#[tauri::command]
pub async fn resize(state: State<'_, AppState>, cols: u32, rows: u32) -> Result<(), String> {
    resize_inner(&state, cols, rows).await.map_err(Into::into)
}

async fn resize_inner(state: &AppState, cols: u32, rows: u32) -> Result<(), AppError> {
    let cols = cols.max(20);
    let rows = rows.max(5);
    let (prev_c, prev_r) = state.term_size();
    state.set_term_size(cols, rows);

    // Skip no-op / tiny jitter resizes to avoid flooding the interactive shell
    // with `stty` commands (was racing with user input after connect).
    if prev_c == cols && prev_r == rows {
        return Ok(());
    }

    let (stdin, alive) = {
        let transport = state.transport.lock().expect("transport lock");
        match transport.as_ref() {
            Some(t) => t.clone_writer(),
            None => return Ok(()),
        }
    };

    // Debounce: only push remote stty if last inject was > 800ms ago.
    {
        use std::time::{Duration, Instant};
        static LAST_STTY: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
        let mut g = LAST_STTY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t0) = *g {
            if t0.elapsed() < Duration::from_millis(800) {
                crate::ops_log::log(
                    "SSH",
                    &format!("resize debounced cols={cols} rows={rows} (local only)"),
                );
                return Ok(());
            }
        }
        *g = Some(Instant::now());
    }

    // Best-effort remote stty (OpenSSH pipe has no SIGWINCH).
    let cmd = format!("stty cols {cols} rows {rows} 2>/dev/null\r");
    crate::ops_log::log("SSH", &format!("resize → remote stty cols={cols} rows={rows}"));
    let _ = write_stdin(&stdin, &alive, cmd.as_bytes()).await;
    Ok(())
}

#[tauri::command]
pub async fn get_session_snapshot(
    state: State<'_, AppState>,
) -> Result<crate::app_state::SessionSnapshot, String> {
    Ok(state.snapshot())
}

// --- Profile commands ---

#[tauri::command]
pub async fn list_profiles() -> Result<Vec<HostProfile>, String> {
    config::list_profiles().map_err(Into::into)
}

#[tauri::command]
pub async fn save_profile(req: SaveProfileRequest) -> Result<HostProfile, String> {
    save_profile_inner(req).map_err(Into::into)
}

fn save_profile_inner(req: SaveProfileRequest) -> Result<HostProfile, AppError> {
    let mut profile = if let Some(id) = req.id {
        let mut existing = config::get_profile(&id)?
            .ok_or_else(|| AppError::Config(format!("配置不存在: {id}")))?;
        existing.name = req.name;
        existing.host = req.host;
        existing.port = req.port;
        existing.username = req.username;
        existing.auth_type = req.auth_type;
        existing.private_key_path = req.private_key_path;
        existing
    } else {
        HostProfile::new(
            req.name,
            req.host,
            req.port,
            req.username,
            req.auth_type,
            req.private_key_path,
        )
    };

    if req.auth_type == AuthType::Password {
        if req.save_password {
            if let Some(pw) = req.password.as_ref().filter(|p| !p.is_empty()) {
                credentials::save_password(&profile.id, pw)?;
                profile.has_saved_password = true;
            }
        }
    } else {
        credentials::delete_password(&profile.id)?;
        profile.has_saved_password = false;
    }

    config::upsert_profile(profile)
}

#[tauri::command]
pub async fn delete_profile(id: String) -> Result<(), String> {
    config::delete_profile(&id).map_err(Into::into)
}

// silence unused import warning if CwdTracker only used via path
#[allow(dead_code)]
fn _cwd_type(_: &CwdTracker) {}
