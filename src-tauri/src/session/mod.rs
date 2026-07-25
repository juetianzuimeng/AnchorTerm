//! Session commands and reconnect / cwd-restore playbooks.
//!
//! PR2: all session commands require client-generated `session_id`.
//! Backend has no default / current session.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tracing::{debug, info, warn};

use crate::app_state::{
    AppState, CachedConnect, CwdEvent, SessionRuntime, SessionSnapshot, SessionState,
};
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

fn emit_state(app: &AppHandle, rt: &SessionRuntime) {
    let _ = app.emit("session://state", rt.snapshot());
}

fn emit_cwd(app: &AppHandle, rt: &SessionRuntime, path: &str) {
    let _ = app.emit(
        "session://cwd",
        CwdEvent {
            session_id: rt.id.clone(),
            cwd: path.to_string(),
        },
    );
}

fn set_state(app: &AppHandle, rt: &SessionRuntime, next: SessionState, message: Option<String>) {
    {
        let mut meta = rt.meta.lock().expect("meta lock");
        meta.state = next;
        meta.message = message;
    }
    emit_state(app, rt);
}

#[derive(Debug, Deserialize)]
pub struct ConnectRequest {
    /// Client-generated UUID (required). Inserted into the map before any emit.
    pub session_id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub cols: u32,
    pub rows: u32,
    pub profile_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ConnectResponse {
    pub session_id: String,
}

#[tauri::command]
pub async fn connect(
    app: AppHandle,
    state: State<'_, AppState>,
    mut req: ConnectRequest,
) -> Result<ConnectResponse, String> {
    if req.session_id.trim().is_empty() {
        return Err(AppError::Message("session_id 不能为空".into()).into());
    }
    // Insert (or reuse) runtime BEFORE any Connecting emit / SSH spawn.
    let rt = state
        .get_or_insert_runtime(req.session_id.trim())
        .map_err(|e| -> String { e.into() })?;
    // Normalize stored id from runtime.
    req.session_id = rt.id.clone();

    connect_inner(&app, rt, &mut req, false)
        .await
        .map_err(|e| -> String { e.into() })?;
    Ok(ConnectResponse {
        session_id: req.session_id,
    })
}

async fn connect_inner(
    app: &AppHandle,
    rt: Arc<SessionRuntime>,
    req: &mut ConnectRequest,
    is_reconnect: bool,
) -> Result<(), AppError> {
    {
        let transport = rt.transport.lock().expect("transport lock");
        if transport.is_some() {
            return Err(AppError::AlreadyConnected);
        }
    }

    resolve_password(req)?;

    rt.set_term_size(req.cols, req.rows);

    {
        let mut meta = rt.meta.lock().expect("meta lock");
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
        rt.auto_reconnect.store(true, Ordering::SeqCst);
        // Invalidate any old reconnect loops.
        rt.reconnect_gen.fetch_add(1, Ordering::SeqCst);

        // Same host+user as last session + absolute restore path → keep and restore.
        // Different endpoint or no path → clear (fresh login home).
        let prev = rt.cached.lock().ok().and_then(|c| c.clone()).or_else(|| {
            let meta = rt.meta.lock().ok()?;
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
            rt.restore_target
                .lock()
                .ok()
                .and_then(|r| r.clone())
                .or_else(|| {
                    rt.cwd
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
            if let Ok(mut target) = rt.restore_target.lock() {
                *target = Some(path.clone());
            }
            if let Ok(mut cwd) = rt.cwd.lock() {
                cwd.set(path.clone());
            }
            crate::ops_log::log(
                "CWD",
                &format!(
                    "user reconnect will restore path={path} (same host/user) sid={}",
                    &rt.id[..rt.id.len().min(8)]
                ),
            );
        } else {
            if let Ok(mut cwd) = rt.cwd.lock() {
                cwd.clear();
            }
            if let Ok(mut target) = rt.restore_target.lock() {
                *target = None;
            }
            crate::ops_log::log("CWD", "user connect: no restore target (fresh cwd)");
        }

        rt.cwd_freeze.store(false, Ordering::SeqCst);
        set_state(
            app,
            &rt,
            SessionState::Connecting,
            Some("正在连接…".into()),
        );
    }

    // Cache credentials in memory for reconnect (never written to disk here).
    {
        let mut cached = rt.cached.lock().expect("cached lock");
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
            "connect_inner begin sid={} reconnect={is_reconnect} host={} port={} user={} cols={} rows={}",
            &rt.id[..rt.id.len().min(8)],
            req.host,
            req.port,
            req.username,
            req.cols,
            req.rows
        ),
    );

    match connect_session(app.clone(), params, rt.id.clone()).await {
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

            *rt.transport.lock().expect("transport lock") = Some(transport);

            {
                let mut meta = rt.meta.lock().expect("meta lock");
                meta.attempt = 0;
            }
            set_state(
                app,
                &rt,
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
                run_restore_playbook(app, &rt).await;
            } else {
                // Seed so relative `cd foo` can be resolved after first login.
                schedule_seed_login_pwd(app, Arc::clone(&rt), 900);
            }
            Ok(())
        }
        Err(e) => {
            crate::ops_log::log("ERR", &format!("connect failed: {e}"));
            if !is_reconnect {
                set_state(app, &rt, SessionState::Failed, Some(e.to_string()));
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

fn session_still_connected(rt: &SessionRuntime) -> bool {
    let meta = rt.meta.lock().expect("meta lock");
    matches!(meta.state, SessionState::Connected)
}

async fn send_pty_bytes(rt: &SessionRuntime, data: Vec<u8>) -> bool {
    let (stdin, alive) = {
        let g = rt.transport.lock().expect("transport lock");
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
async fn run_restore_playbook(app: &AppHandle, rt: &SessionRuntime) {
    // Prefer path frozen at disconnect — live last_known may already be $HOME
    // from the new login shell's OSC 7.
    let path = rt
        .restore_target
        .lock()
        .ok()
        .and_then(|r| r.clone())
        .or_else(|| {
            rt.cwd
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
    rt.cwd_freeze.store(true, Ordering::SeqCst);
    if let Ok(mut tracker) = rt.cwd.lock() {
        tracker.set(path.clone());
    }
    emit_cwd(app, rt, &path);

    // Give login shell time to finish .bashrc / print banner before injecting cd.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    if !session_still_connected(rt) {
        rt.cwd_freeze.store(false, Ordering::SeqCst);
        return;
    }

    // Existence check via side-channel exec is fine (filesystem, not interactive cwd).
    let dir_ok = query_dir_exists(rt, &path).await.unwrap_or(true);
    if !dir_ok {
        warn!(path = %path, "restore target missing on remote; fall back to login home");
        rt.cwd_freeze.store(false, Ordering::SeqCst);
        // Drop poisoned path so the next disconnect does not re-freeze it.
        if let Ok(mut target) = rt.restore_target.lock() {
            *target = None;
        }
        if let Ok(mut tracker) = rt.cwd.lock() {
            tracker.clear();
        }
        emit_cwd(app, rt, "");
        set_state(
            app,
            rt,
            SessionState::Connected,
            Some(format!(
                "重连成功；原目录不可用（{path}），已改用登录目录"
            )),
        );
        crate::ops_log::log(
            "CWD",
            &format!("restore skipped missing path={path}; seeding login pwd"),
        );
        // Seed absolute $HOME via side-channel pwd (new process = login home only).
        if let Some(state) = app.try_state::<AppState>() {
            if let Ok(arc) = state.get_runtime(&rt.id) {
                schedule_seed_login_pwd(app, arc, 400);
            }
        }
        return;
    }

    let cmd = restore_cd_command(&path);
    info!(path = %path, "restore playbook: silent cd (first send)");

    if !send_pty_bytes(rt, cmd.clone().into_bytes()).await {
        rt.cwd_freeze.store(false, Ordering::SeqCst);
        set_state(
            app,
            rt,
            SessionState::Connected,
            Some(format!("重连成功，但无法发送恢复目录命令: {path}")),
        );
        return;
    }

    // Allow the line discipline to run `cd`.
    tokio::time::sleep(Duration::from_millis(550)).await;
    if !session_still_connected(rt) {
        rt.cwd_freeze.store(false, Ordering::SeqCst);
        return;
    }

    // Observe OSC 7 (if installed). Side-channel pwd cannot see interactive cwd.
    rt.cwd_freeze.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(800)).await;
    if !session_still_connected(rt) {
        return;
    }

    let observed = rt
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
        rt.cwd_freeze.store(true, Ordering::SeqCst);
        if !send_pty_bytes(rt, cmd.into_bytes()).await {
            rt.cwd_freeze.store(false, Ordering::SeqCst);
            set_state(
                app,
                rt,
                SessionState::Connected,
                Some(format!("重连成功，二次恢复目录失败: {path}")),
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(450)).await;
        rt.cwd_freeze.store(false, Ordering::SeqCst);
    }

    // Bookkeeping: prefer target path for next disconnect restore.
    if let Ok(mut tracker) = rt.cwd.lock() {
        tracker.set(path.clone());
    }
    if let Ok(mut target) = rt.restore_target.lock() {
        *target = Some(path.clone());
    }
    emit_cwd(app, rt, &path);
    set_state(
        app,
        rt,
        SessionState::Connected,
        Some(format!("已恢复工作目录: {path}")),
    );
    info!(path = %path, retry = need_retry, "restore playbook done");
    rt.cwd_freeze.store(false, Ordering::SeqCst);
}

fn connect_params_from_cache(rt: &SessionRuntime) -> Result<ConnectParams, String> {
    let cached = rt
        .cached
        .lock()
        .expect("cached lock")
        .clone()
        .ok_or_else(|| "无会话凭据缓存".to_string())?;
    let (cols, rows) = rt.term_size();
    Ok(ConnectParams {
        host: cached.host,
        port: cached.port,
        username: cached.username,
        auth: cached.auth,
        cols,
        rows,
    })
}

async fn query_remote_pwd(rt: &SessionRuntime) -> Result<String, String> {
    let params = connect_params_from_cache(rt)?;
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

async fn query_dir_exists(rt: &SessionRuntime, path: &str) -> Result<bool, String> {
    let params = connect_params_from_cache(rt)?;
    let quoted = crate::cwd::shell_single_quote(path);
    let cmd = format!("test -d {quoted} && echo AT_DIR_OK || echo AT_DIR_MISSING");
    let out = crate::ssh::openssh::openssh_exec(&params, &cmd)
        .await
        .map_err(|e| e.to_string())?;
    Ok(out.contains("AT_DIR_OK"))
}

/// Seed last_known from a fresh login shell's pwd (initial home). Safe only when
/// the interactive shell has not yet changed directory.
fn schedule_seed_login_pwd(app: &AppHandle, rt: Arc<SessionRuntime>, delay_ms: u64) {
    let app = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        if rt.cwd_freeze.load(Ordering::SeqCst) {
            return;
        }
        {
            let meta = rt.meta.lock().expect("meta lock");
            if !matches!(meta.state, SessionState::Connected) {
                return;
            }
        }
        // Only seed if we still have no absolute path.
        {
            let cwd = rt.cwd.lock().expect("cwd lock");
            if cwd
                .last_known()
                .map(|p| p.starts_with('/'))
                .unwrap_or(false)
            {
                return;
            }
        }
        match query_remote_pwd(&rt).await {
            Ok(path) => {
                if let Ok(mut tracker) = rt.cwd.lock() {
                    tracker.set(path.clone());
                }
                if let Ok(mut target) = rt.restore_target.lock() {
                    *target = Some(path.clone());
                }
                emit_cwd(&app, &rt, &path);
                emit_state(&app, &rt);
                debug!("seeded login pwd");
            }
            Err(e) => debug!(error = %e, "seed login pwd failed"),
        }
    });
}

/// Permanent credential/key problems — do not spin forever.
/// Transient network/handshake errors must return false (keep auto-reconnect).
fn is_permanent_auth_failure(e: &AppError) -> bool {
    match e {
        AppError::Auth(msg) => {
            let l = msg.to_ascii_lowercase();
            // Explicit network wording in Auth messages → still retry (belt & suspenders).
            if l.contains("connection closed")
                || l.contains("connection reset")
                || l.contains("timed out")
                || l.contains("network")
                || l.contains("握手")
            {
                return false;
            }
            true
        }
        // Config/path problems that will not self-heal without user action.
        AppError::Config(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod permanent_auth_tests {
    use super::*;

    #[test]
    fn connect_error_is_not_permanent() {
        let e = AppError::Connect("网络或握手中断: Connection closed by host".into());
        assert!(!is_permanent_auth_failure(&e));
    }

    #[test]
    fn real_auth_is_permanent() {
        let e = AppError::Auth("认证失败：公钥被拒绝或私钥口令错误。".into());
        assert!(is_permanent_auth_failure(&e));
    }
}

/// Spawn background reconnect with exponential backoff for a specific session.
pub fn spawn_reconnect_loop(app: AppHandle, session_id: String) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Ok(rt) = state.get_runtime(&session_id) else {
        return;
    };
    if !rt.auto_reconnect.load(Ordering::SeqCst) {
        return;
    }

    let gen = rt.reconnect_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let app2 = app.clone();

    tokio::spawn(async move {
        reconnect_loop(app2, rt, gen).await;
    });
}

async fn reconnect_loop(app: AppHandle, rt: Arc<SessionRuntime>, gen: u64) {
    let mut attempt: u32 = 0;

    loop {
        if rt.reconnect_gen.load(Ordering::SeqCst) != gen {
            return; // cancelled
        }
        if !rt.auto_reconnect.load(Ordering::SeqCst) {
            return;
        }
        // Already connected by someone else.
        if rt.transport.lock().expect("t").is_some() {
            return;
        }

        attempt = attempt.saturating_add(1);
        let delay = BACKOFF_SECS[(attempt as usize - 1).min(BACKOFF_SECS.len() - 1)];

        {
            let mut meta = rt.meta.lock().expect("meta lock");
            meta.state = SessionState::Reconnecting;
            meta.attempt = attempt;
            meta.message = Some(format!("重连中 ({attempt})，{delay}s 后重试…"));
        }
        emit_state(&app, &rt);

        tokio::time::sleep(Duration::from_secs(delay)).await;

        if rt.reconnect_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        if !rt.auto_reconnect.load(Ordering::SeqCst) {
            return;
        }
        if rt.transport.lock().expect("t").is_some() {
            return;
        }

        let cached = rt.cached.lock().expect("cached").clone();
        let Some(cached) = cached else {
            set_state(
                &app,
                &rt,
                SessionState::Failed,
                Some("无法自动重连：会话凭据已丢失，请重新连接".into()),
            );
            return;
        };

        let (cols, rows) = rt.term_size();
        let mut req = ConnectRequest {
            session_id: rt.id.clone(),
            host: cached.host,
            port: cached.port,
            username: cached.username,
            auth: cached.auth,
            cols,
            rows,
            profile_id: cached.profile_id,
        };

        {
            let mut meta = rt.meta.lock().expect("meta lock");
            meta.message = Some(format!("重连中 ({attempt})…"));
        }
        emit_state(&app, &rt);

        match connect_inner(&app, Arc::clone(&rt), &mut req, true).await {
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
                // Only **real** credential/key failures stop the loop.
                // Network blips ("Connection closed by host", reset, timeout) are
                // AppError::Connect and must keep retrying after the cable is back.
                if is_permanent_auth_failure(&e) {
                    rt.auto_reconnect.store(false, Ordering::SeqCst);
                    set_state(
                        &app,
                        &rt,
                        SessionState::Failed,
                        Some(format!("认证失败，已停止自动重连: {e}")),
                    );
                    return;
                }
                {
                    let mut meta = rt.meta.lock().expect("meta lock");
                    meta.state = SessionState::Reconnecting;
                    meta.attempt = attempt;
                    meta.message = Some(format!("重连失败 ({attempt}): {e}"));
                }
                emit_state(&app, &rt);
            }
        }
    }
}

#[tauri::command]
pub async fn disconnect(
    state: State<'_, AppState>,
    app: AppHandle,
    session_id: String,
) -> Result<(), String> {
    let rt = state.get_runtime(&session_id).map_err(|e| -> String { e.into() })?;
    disconnect_inner(&rt, &app).map_err(Into::into)
}

/// Close tab: cancel reconnect, drop transport (temp key), remove from map.
/// Order per design §3.4.1.
#[tauri::command]
pub async fn close_session(
    state: State<'_, AppState>,
    app: AppHandle,
    session_id: String,
) -> Result<(), String> {
    close_session_inner(&state, &app, &session_id).map_err(Into::into)
}

/// Graceful app shutdown: tear down all sessions, then force-exit the process.
///
/// Prefer this over only `Window.close()`: close-requested handlers that
/// re-enter `close()` can deadlock, and leftover OpenSSH children may keep
/// the process alive after the window disappears.
#[tauri::command]
pub async fn app_quit(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    crate::ops_log::log("SYS", "app_quit begin");
    let ids = state.list_session_ids();
    for id in ids {
        if let Err(e) = close_session_inner(&state, &app, &id) {
            crate::ops_log::log(
                "ERR",
                &format!(
                    "app_quit close_session failed sid={} err={e}",
                    &id[..id.len().min(8)]
                ),
            );
        }
    }
    crate::ops_log::log("SYS", "app_quit exit(0)");
    // Give the log a moment to flush to disk.
    tokio::time::sleep(Duration::from_millis(30)).await;
    app.exit(0);
    Ok(())
}

fn close_session_inner(
    state: &AppState,
    app: &AppHandle,
    session_id: &str,
) -> Result<(), AppError> {
    let rt = state.get_runtime(session_id)?;
    crate::ops_log::log(
        "UI",
        &format!(
            "close_session sid={}",
            &session_id[..session_id.len().min(8)]
        ),
    );
    // 1–4: cancel reconnect + clear freeze
    rt.auto_reconnect.store(false, Ordering::SeqCst);
    rt.reconnect_gen.fetch_add(1, Ordering::SeqCst);
    rt.cwd_freeze.store(false, Ordering::SeqCst);
    // 5: take transport → Drop SecureKeyMaterial
    let transport = rt.transport.lock().expect("transport lock").take();
    if let Some(t) = transport {
        let _ = t.cmd_tx.send(SessionCommand::Disconnect);
    }
    // 7: remove from map last
    state.remove_runtime(session_id);
    // Notify UI (optional snapshot of gone session — emit idle then gone)
    let _ = app.emit(
        "session://state",
        SessionSnapshot {
            session_id: session_id.to_string(),
            state: SessionState::Idle,
            host: None,
            username: None,
            message: Some("会话已关闭".into()),
            cwd: None,
            attempt: None,
        },
    );
    crate::ops_log::log(
        "STATE",
        &format!(
            "close_session removed sid={}",
            &session_id[..session_id.len().min(8)]
        ),
    );
    Ok(())
}

#[tauri::command]
pub async fn list_sessions(
    state: State<'_, AppState>,
) -> Result<Vec<SessionSnapshot>, String> {
    Ok(state.list_snapshots())
}

fn disconnect_inner(rt: &SessionRuntime, app: &AppHandle) -> Result<(), AppError> {
    crate::ops_log::log("UI", "disconnect manual");
    // Disable auto-reconnect for this session (manual disconnect).
    rt.auto_reconnect.store(false, Ordering::SeqCst);
    rt.reconnect_gen.fetch_add(1, Ordering::SeqCst);

    // Freeze absolute cwd so the next user-initiated connect can restore it.
    freeze_restore_target_from_cwd(rt, "manual disconnect");

    let transport = rt.transport.lock().expect("transport lock").take();
    if let Some(t) = transport {
        let _ = t.cmd_tx.send(SessionCommand::Disconnect);
    }

    {
        let mut meta = rt.meta.lock().expect("meta lock");
        meta.state = SessionState::Idle;
        meta.message = Some("已手动断开".into());
        meta.attempt = 0;
    }
    emit_state(app, rt);
    crate::ops_log::log("STATE", "idle (manual disconnect)");
    Ok(())
}

/// Remember absolute cwd for a later reconnect / user re-connect restore.
fn freeze_restore_target_from_cwd(rt: &SessionRuntime, reason: &str) {
    let path = rt
        .cwd
        .lock()
        .ok()
        .and_then(|c| c.last_known().map(|s| s.to_string()))
        .filter(|p| p.starts_with('/'));
    if let Some(path) = path {
        if let Ok(mut target) = rt.restore_target.lock() {
            *target = Some(path.clone());
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
pub async fn write_bytes(
    state: State<'_, AppState>,
    session_id: String,
    data_b64: String,
) -> Result<(), String> {
    let rt = state.get_runtime(&session_id).map_err(|e| -> String { e.into() })?;
    write_inner(&rt, data_b64).await.map_err(Into::into)
}

async fn write_inner(rt: &SessionRuntime, data_b64: String) -> Result<(), AppError> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| AppError::Message(format!("无效的 base64 输入: {e}")))?;

    // Track cd from submitted lines (fallback when no OSC 7). Optimistic — may roll back.
    if let Ok(text) = std::str::from_utf8(&bytes) {
        for line in text.split(['\n', '\r']) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(mut cwd) = rt.cwd.lock() {
                if let Some(ch) = cwd.feed_submitted_line(line) {
                    if let Some(path) = ch.path.filter(|p| p.starts_with('/')) {
                        if let Ok(mut target) = rt.restore_target.lock() {
                            *target = Some(path);
                        }
                    }
                }
            }
        }
    }

    let (stdin, alive) = {
        let transport = rt.transport.lock().expect("transport lock");
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
    session_id: String,
    line: String,
) -> Result<(), String> {
    let rt = state.get_runtime(&session_id).map_err(|e| -> String { e.into() })?;
    submit_line_inner(&rt, &app, line)
        .await
        .map_err(Into::into)
}

async fn submit_line_inner(
    rt: &SessionRuntime,
    app: &AppHandle,
    line: String,
) -> Result<(), AppError> {
    // Draft is one logical shell line.
    let logical = line.trim_end_matches(['\n', '\r']);
    if logical.is_empty() {
        return Ok(());
    }

    let (stdin, alive) = {
        let transport = rt.transport.lock().expect("transport lock");
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

    // Track cwd from the submitted line (optimistic; OSC 7 / failure echo may correct).
    // IMPORTANT: release `cwd` before emit_state/snapshot — snapshot() also locks cwd
    // and std::sync::Mutex is not reentrant (deadlock froze the stdout pump after `cd`).
    let change = {
        let mut cwd = rt.cwd.lock().expect("cwd lock");
        cwd.feed_submitted_line(logical)
    };
    if let Some(ch) = change {
        if let Some(ref path) = ch.path {
            crate::ops_log::log("CWD", &format!("from_cd_parse path={path}"));
            if path.starts_with('/') {
                if let Ok(mut target) = rt.restore_target.lock() {
                    *target = Some(path.clone());
                }
            }
            emit_cwd(app, rt, path);
        }
        emit_state(app, rt);
    }

    Ok(())
}

/// Tab completion for the draft input (remote compgen via side-channel OpenSSH exec).
#[tauri::command]
pub async fn complete_draft(
    state: State<'_, AppState>,
    session_id: String,
    line: String,
    cursor: usize,
) -> Result<crate::ssh::complete::CompleteResult, String> {
    let rt = state.get_runtime(&session_id).map_err(|e| -> String { e.into() })?;
    complete_draft_inner(&rt, line, cursor).await
}

async fn complete_draft_inner(
    rt: &SessionRuntime,
    line: String,
    cursor: usize,
) -> Result<crate::ssh::complete::CompleteResult, String> {
    {
        let meta = rt.meta.lock().expect("meta lock");
        if !matches!(meta.state, SessionState::Connected) {
            return Err("未连接，无法补全".into());
        }
    }

    let cwd = rt
        .cwd
        .lock()
        .ok()
        .and_then(|c| c.last_known().map(|s| s.to_string()));

    let params = connect_params_from_cache(rt)?;

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
pub async fn resize(
    state: State<'_, AppState>,
    session_id: String,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    let rt = state.get_runtime(&session_id).map_err(|e| -> String { e.into() })?;
    resize_inner(&rt, cols, rows).await.map_err(Into::into)
}

async fn resize_inner(rt: &SessionRuntime, cols: u32, rows: u32) -> Result<(), AppError> {
    let cols = cols.max(20);
    let rows = rows.max(5);
    rt.set_term_size(cols, rows);

    // Skip if this size was already injected to the remote PTY.
    // (Local cols/rows are set at connect *before* any stty — do not use those alone.)
    {
        let remote = rt
            .remote_stty
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *remote == Some((cols, rows)) {
            return Ok(());
        }
    }

    let (stdin, alive) = {
        let transport = rt.transport.lock().expect("transport lock");
        match transport.as_ref() {
            Some(t) => t.clone_writer(),
            None => return Ok(()),
        }
    };

    // Debounce: only push remote stty if last inject was > 800ms ago (per session).
    {
        use std::time::{Duration, Instant};
        let mut g = rt.last_stty.lock().unwrap_or_else(|e| e.into_inner());
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

    // Best-effort remote stty (OpenSSH pipe has no SIGWINCH). Echo is stripped
    // in on_data via echo_suppress so the user never sees the control command.
    let cmd = format!("stty cols {cols} rows {rows} 2>/dev/null\r");
    rt.arm_stty_echo_suppress(cols, rows);
    crate::ops_log::log(
        "SSH",
        &format!("resize → silent remote stty cols={cols} rows={rows}"),
    );
    let _ = write_stdin(&stdin, &alive, cmd.as_bytes()).await;
    if let Ok(mut g) = rt.remote_stty.lock() {
        *g = Some((cols, rows));
    }
    Ok(())
}

#[tauri::command]
pub async fn get_session_snapshot(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<SessionSnapshot, String> {
    let rt = state.get_runtime(&session_id).map_err(|e| -> String { e.into() })?;
    Ok(rt.snapshot())
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
