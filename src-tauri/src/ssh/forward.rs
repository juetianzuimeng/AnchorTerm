//! SSH port forwarding (`ssh -N -L` / `ssh -N -R`), attached to a session tab.
//!
//! Each forward is a **separate** OpenSSH process (same credentials as the tab):
//! - Local (`-L`): `ssh -N -L [bind:]listen:dest_host:dest_port user@host -p port`
//! - Remote (`-R`): `ssh -N -R [bind:]listen:dest_host:dest_port user@host -p port`

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

use crate::app_state::{AppState, SessionRuntime, SessionState};
use crate::error::AppError;
use crate::ssh::openssh::{self, ForwardFlag, RunningLocalForward};
use crate::ssh::transport::ConnectParams;

const MAX_FORWARDS_PER_SESSION: usize = 8;
const DEFAULT_BIND: &str = "127.0.0.1";
const DEFAULT_DEST: &str = "127.0.0.1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    Local,
    Remote,
}

impl Default for ForwardKind {
    fn default() -> Self {
        ForwardKind::Local
    }
}

impl ForwardKind {
    fn flag(self) -> ForwardFlag {
        match self {
            ForwardKind::Local => ForwardFlag::Local,
            ForwardKind::Remote => ForwardFlag::Remote,
        }
    }

    fn ssh_flag(self) -> &'static str {
        self.flag().as_ssh()
    }

    fn label(self) -> &'static str {
        match self {
            ForwardKind::Local => "本地",
            ForwardKind::Remote => "远程",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForwardState {
    Starting,
    Listening,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardSpec {
    pub id: String,
    #[serde(default)]
    pub kind: ForwardKind,
    #[serde(default = "default_bind")]
    pub bind_address: String,
    pub listen_port: u16,
    #[serde(default = "default_dest")]
    pub dest_host: String,
    pub dest_port: u16,
}

fn default_bind() -> String {
    DEFAULT_BIND.to_string()
}

fn default_dest() -> String {
    DEFAULT_DEST.to_string()
}

impl ForwardSpec {
    /// OpenSSH `-L`/`-R` spec. Loopback bind is omitted so the argv matches
    /// `ssh -L 6379:host:6379` — Windows OpenSSH often fails
    /// `bind [127.0.0.1]:port: Permission denied` when the address is explicit.
    pub fn ssh_arg(&self) -> String {
        let dest = format!(
            "{}:{}:{}",
            self.listen_port, self.dest_host, self.dest_port
        );
        if bind_is_implicit_loopback(&self.bind_address) {
            dest
        } else {
            format!("{}:{}", self.bind_address, dest)
        }
    }

    pub fn display(&self) -> String {
        format!(
            "{} {} {}:{} → {}:{}",
            self.kind.ssh_flag(),
            self.kind.label(),
            self.bind_address,
            self.listen_port,
            self.dest_host,
            self.dest_port
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ForwardInfo {
    pub id: String,
    pub kind: ForwardKind,
    pub bind_address: String,
    pub listen_port: u16,
    pub dest_host: String,
    pub dest_port: u16,
    pub state: ForwardState,
    pub message: Option<String>,
    pub ssh_arg: String,
    /// Alias of [`Self::ssh_arg`] for older UI that read `l_arg`.
    pub l_arg: String,
}

impl ForwardInfo {
    fn from_spec(spec: &ForwardSpec, state: ForwardState, message: Option<String>) -> Self {
        let ssh_arg = spec.ssh_arg();
        Self {
            id: spec.id.clone(),
            kind: spec.kind,
            bind_address: spec.bind_address.clone(),
            listen_port: spec.listen_port,
            dest_host: spec.dest_host.clone(),
            dest_port: spec.dest_port,
            state,
            message,
            l_arg: ssh_arg.clone(),
            ssh_arg,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ForwardsEvent {
    pub session_id: String,
    pub forwards: Vec<ForwardInfo>,
    /// Whether auto-restore is enabled for this session's endpoint.
    #[serde(default)]
    pub auto_restore: bool,
}

struct LiveForward {
    spec: ForwardSpec,
    handle: Arc<RunningLocalForward>,
}

struct ForwardSetInner {
    desired: Vec<ForwardSpec>,
    live: HashMap<String, LiveForward>,
    last_error: HashMap<String, String>,
    /// Ids whose last spawn failed permanently (port in use / AllowTcpForwarding
    /// denied / agent-only auth). Not retried by `restart_dead_forwards`.
    permanent_failed: HashSet<String>,
}

pub struct ForwardSet {
    inner: Mutex<ForwardSetInner>,
}

impl Default for ForwardSet {
    fn default() -> Self {
        Self {
            inner: Mutex::new(ForwardSetInner {
                desired: Vec::new(),
                live: HashMap::new(),
                last_error: HashMap::new(),
                permanent_failed: HashSet::new(),
            }),
        }
    }
}

impl ForwardSet {
    pub fn snapshot(&self) -> Vec<ForwardInfo> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.desired
            .iter()
            .map(|spec| {
                let (state, message) = if let Some(live) = g.live.get(&spec.id) {
                    if live.handle.alive.load(Ordering::SeqCst) {
                        (ForwardState::Listening, None)
                    } else {
                        (
                            ForwardState::Failed,
                            g.last_error.get(&spec.id).cloned().or_else(|| {
                                Some("隧道进程已退出".into())
                            }),
                        )
                    }
                } else if let Some(err) = g.last_error.get(&spec.id) {
                    (ForwardState::Failed, Some(err.clone()))
                } else {
                    (ForwardState::Stopped, None)
                };
                ForwardInfo::from_spec(spec, state, message)
            })
            .collect()
    }

    fn has_listen(&self, kind: ForwardKind, port: u16) -> bool {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.desired
            .iter()
            .any(|s| s.kind == kind && s.listen_port == port)
            || g.live.values().any(|l| {
                l.spec.kind == kind
                    && l.spec.listen_port == port
                    && l.handle.alive.load(Ordering::SeqCst)
            })
    }

    pub fn kill_all(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for live in g.live.values() {
            live.handle.start_kill();
        }
        g.live.clear();
        g.desired.clear();
        g.last_error.clear();
        g.permanent_failed.clear();
    }

    fn take_dead_desired(&self) -> Vec<ForwardSpec> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.desired
            .iter()
            .filter(|s| {
                g.live
                    .get(&s.id)
                    .map(|l| !l.handle.alive.load(Ordering::SeqCst))
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }
}

pub fn parse_spec(
    kind: ForwardKind,
    bind_address: Option<&str>,
    listen_port: u16,
    dest_host: &str,
    dest_port: u16,
) -> Result<ForwardSpec, AppError> {
    if listen_port == 0 {
        return Err(AppError::Message(match kind {
            ForwardKind::Local => "本机端口无效".into(),
            ForwardKind::Remote => "远端监听端口无效".into(),
        }));
    }
    if dest_port == 0 {
        return Err(AppError::Message(match kind {
            ForwardKind::Local => "远端端口无效".into(),
            ForwardKind::Remote => "本机目标端口无效".into(),
        }));
    }
    let bind_label = match kind {
        ForwardKind::Local => "本机监听地址",
        ForwardKind::Remote => "远端监听地址",
    };
    let dest_label = match kind {
        ForwardKind::Local => "远端主机",
        ForwardKind::Remote => "本机目标主机",
    };
    let bind = normalize_host(bind_address.unwrap_or(""), DEFAULT_BIND, bind_label)?;
    let dest = normalize_host(dest_host, DEFAULT_DEST, dest_label)?;
    if dest.is_empty() {
        return Err(AppError::Message(format!(
            "请填写{dest_label}（默认 {DEFAULT_DEST}）"
        )));
    }
    Ok(ForwardSpec {
        id: Uuid::new_v4().to_string(),
        kind,
        bind_address: bind,
        listen_port,
        dest_host: dest,
        dest_port,
    })
}

fn bind_is_implicit_loopback(bind: &str) -> bool {
    let t = bind.trim();
    t.is_empty()
        || t == "127.0.0.1"
        || t.eq_ignore_ascii_case("localhost")
}

fn normalize_host(raw: &str, default: &str, label: &str) -> Result<String, AppError> {
    let t = raw.trim();
    let v = if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    };
    if v.is_empty() {
        return Ok(v);
    }
    if v.chars().any(|c| c.is_whitespace() || c == '/' || c == '%' || c == '\\') {
        return Err(AppError::Message(format!("{label}含有非法字符")));
    }
    if v.contains(':') {
        return Err(AppError::Message(format!(
            "{label}暂不支持 IPv6 或带冒号的值"
        )));
    }
    if v.len() > 253 {
        return Err(AppError::Message(format!("{label}过长")));
    }
    Ok(v)
}

fn emit_forwards(app: &AppHandle, rt: &SessionRuntime) {
    let auto_restore = rt
        .cached
        .lock()
        .ok()
        .and_then(|c| c.clone())
        .map(|c| {
            if c.host.is_empty() || c.username.is_empty() {
                false
            } else {
                crate::config::get_auto_restore(&c.host, c.port, &c.username)
            }
        })
        .unwrap_or(false);
    let _ = app.emit(
        "session://forwards",
        ForwardsEvent {
            session_id: rt.id.clone(),
            forwards: rt.local_forwards.snapshot(),
            auto_restore,
        },
    );
}

fn params_from_connected(rt: &SessionRuntime) -> Result<ConnectParams, AppError> {
    {
        let meta = rt.meta.lock().expect("meta lock");
        if !matches!(meta.state, SessionState::Connected) {
            return Err(AppError::NotConnected);
        }
    }
    let cached = rt
        .cached
        .lock()
        .expect("cached lock")
        .clone()
        .ok_or(AppError::NotConnected)?;
    Ok(ConnectParams {
        host: cached.host,
        port: cached.port,
        username: cached.username,
        auth: cached.auth,
        cols: 80,
        rows: 24,
    })
}

fn local_listen_taken_elsewhere(state: &AppState, port: u16, except_session: &str) -> bool {
    let map = state.sessions.lock().expect("sessions lock");
    for (id, rt) in map.iter() {
        if id == except_session {
            continue;
        }
        if rt.local_forwards.has_listen(ForwardKind::Local, port) {
            return true;
        }
    }
    false
}

fn remote_listen_taken_elsewhere(
    state: &AppState,
    except_session: &str,
    ssh_host: &str,
    ssh_port: u16,
    listen_port: u16,
) -> bool {
    let map = state.sessions.lock().expect("sessions lock");
    for (id, rt) in map.iter() {
        if id == except_session {
            continue;
        }
        let same_target = rt
            .cached
            .lock()
            .ok()
            .and_then(|c| c.clone())
            .map(|c| c.host == ssh_host && c.port == ssh_port)
            .unwrap_or(false);
        if same_target && rt.local_forwards.has_listen(ForwardKind::Remote, listen_port) {
            return true;
        }
    }
    false
}

fn attach_wait_task(app: AppHandle, rt: Arc<SessionRuntime>, id: String, handle: Arc<RunningLocalForward>) {
    tokio::spawn(async move {
        loop {
            if !handle.alive.load(Ordering::SeqCst) {
                break;
            }
            let exited = {
                let mut g = handle.child.lock().unwrap_or_else(|e| e.into_inner());
                match g.as_mut() {
                    Some(c) => match c.try_wait() {
                        Ok(Some(status)) => Some(status),
                        Ok(None) => None,
                        Err(_) => {
                            handle.alive.store(false, Ordering::SeqCst);
                            return;
                        }
                    },
                    None => {
                        handle.alive.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            };
            if let Some(status) = exited {
                handle.alive.store(false, Ordering::SeqCst);
                let err = handle.stderr_snapshot().await;
                let flag = {
                    let g = rt
                        .local_forwards
                        .inner
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    g.desired
                        .iter()
                        .find(|s| s.id == id)
                        .map(|s| s.kind.flag())
                        .unwrap_or(openssh::ForwardFlag::Local)
                };
                let msg = if err.trim().is_empty() {
                    format!("隧道已退出 ({status})")
                } else {
                    openssh::classify_forward_error(flag, &err, status.code()).to_string()
                };
                {
                    let mut g = rt
                        .local_forwards
                        .inner
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    g.live.remove(&id);
                    g.last_error.insert(id.clone(), msg.clone());
                    // Permanent failure (port in use / AllowTcpForwarding denied /
                    // agent-only auth) → do not retry on next reconnect.
                    if forward_failure_is_permanent(&msg) {
                        g.permanent_failed.insert(id.clone());
                    }
                }
                crate::ops_log::log(
                    "SSH",
                    &format!(
                        "port_forward exited sid={} id={} status={status:?}",
                        &rt.id[..rt.id.len().min(8)],
                        &id[..id.len().min(8)]
                    ),
                );
                emit_forwards(&app, &rt);
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
}

async fn spawn_and_attach(
    app: &AppHandle,
    rt: &Arc<SessionRuntime>,
    spec: ForwardSpec,
) -> Result<ForwardInfo, AppError> {
    let params = params_from_connected(rt)?;
    let ssh_arg = spec.ssh_arg();
    let control_path = if openssh::control_master_enabled() {
        rt.control_path_opt()
    } else {
        None
    };

    crate::ops_log::log(
        "SSH",
        &format!(
            "port_forward start sid={} flag={} spec={} user={} host={} port={}",
            &rt.id[..rt.id.len().min(8)],
            spec.kind.ssh_flag(),
            ssh_arg,
            params.username,
            params.host,
            params.port
        ),
    );

    let running = openssh::start_port_forward_ssh(
        &params,
        spec.kind.flag(),
        &ssh_arg,
        &rt.side_channel_key,
        control_path.as_deref(),
    )
    .await?;

    let handle = Arc::new(running);
    {
        let mut g = rt
            .local_forwards
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !g.desired.iter().any(|s| s.id == spec.id) {
            g.desired.push(spec.clone());
        }
        g.last_error.remove(&spec.id);
        g.live.insert(
            spec.id.clone(),
            LiveForward {
                spec: spec.clone(),
                handle: Arc::clone(&handle),
            },
        );
    }
    // Persist on start (id-scoped upsert — safe across concurrent tabs).
    crate::config::upsert_rule(&params.host, params.port, &params.username, &spec);
    attach_wait_task(app.clone(), Arc::clone(rt), spec.id.clone(), handle);
    emit_forwards(app, rt);

    Ok(ForwardInfo::from_spec(
        &spec,
        ForwardState::Listening,
        None,
    ))
}

/// Drop all forwards (tab closed or SSH target changed).
pub fn drop_all(app: Option<&AppHandle>, rt: &SessionRuntime) {
    rt.local_forwards.kill_all();
    if let Some(app) = app {
        emit_forwards(app, rt);
    }
}

/// Re-spawn desired tunnels that are not currently listening (after reconnect).
pub async fn restart_dead_forwards(app: &AppHandle, rt: &Arc<SessionRuntime>) {
    let dead = rt.local_forwards.take_dead_desired();
    if dead.is_empty() {
        return;
    }
    let state_guard = app.state::<AppState>();
    let state = state_guard.inner();
    crate::ops_log::log(
        "SSH",
        &format!(
            "port_forward restart_dead sid={} count={}",
            &rt.id[..rt.id.len().min(8)],
            dead.len()
        ),
    );
    for spec in dead {
        // Permanent failures are never retried.
        if rt
            .local_forwards
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .permanent_failed
            .contains(&spec.id)
        {
            continue;
        }
        // Cross-session port conflict (another tab already bound it): skip for
        // now, but stay retryable in case that tab closes.
        if let Err(e) = validate_listen_conflict(&state, rt, &spec) {
            {
                let mut g = rt
                    .local_forwards
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                g.last_error.insert(spec.id.clone(), e.to_string());
            }
            crate::ops_log::log(
                "FWD",
                &format!(
                    "port_forward skip (conflict) sid={} spec={} err={e}",
                    &rt.id[..rt.id.len().min(8)],
                    spec.ssh_arg()
                ),
            );
            continue;
        }
        if let Err(e) = spawn_and_attach(app, rt, spec.clone()).await {
            {
                let mut g = rt
                    .local_forwards
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                g.last_error.insert(spec.id.clone(), e.to_string());
            }
            crate::ops_log::log(
                "ERR",
                &format!(
                    "port_forward restart failed sid={} spec={} err={e}",
                    &rt.id[..rt.id.len().min(8)],
                    spec.ssh_arg()
                ),
            );
        }
    }
    emit_forwards(app, rt);
}

/// Load this endpoint's persisted rules into `desired` (if auto-restore is on)
/// so `restart_dead_forwards` can spawn them after connect. Called on first
/// connect, after `drop_all` for a changed target. Does NOT persist (disk is
/// the source of truth). Does not emit — the subsequent `restart_dead_forwards`
/// emits once the loaded (still-not-live) rules are picked up, at which point
/// `rt.cached` is already populated.
pub fn load_persisted_for(
    rt: &SessionRuntime,
    host: &str,
    port: u16,
    username: &str,
) {
    let ep = match crate::config::load_persisted(host, port, username) {
        Some(ep) => ep,
        None => return,
    };
    if !ep.auto_restore {
        return;
    }
    let count = ep.rules.len();
    {
        let mut g = rt
            .local_forwards
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for spec in ep.rules {
            let dup = g
                .desired
                .iter()
                .any(|s| s.kind == spec.kind && s.listen_port == spec.listen_port);
            if !dup {
                g.desired.push(spec);
            }
        }
    }
    crate::ops_log::log(
        "FWD",
        &format!(
            "forwards auto-restore loaded sid={} host={} user={} count={}",
            &rt.id[..rt.id.len().min(8)],
            host,
            username,
            count
        ),
    );
}

#[derive(Debug, Deserialize)]
pub struct StartPortForwardRequest {
    pub session_id: String,
    #[serde(default)]
    pub kind: ForwardKind,
    pub listen_port: u16,
    pub dest_host: String,
    pub dest_port: u16,
    pub bind_address: Option<String>,
}

#[tauri::command]
pub async fn start_local_forward(
    app: AppHandle,
    state: State<'_, AppState>,
    req: StartPortForwardRequest,
) -> Result<ForwardInfo, String> {
    start_port_forward_inner(&app, &state, req)
        .await
        .map_err(Into::into)
}

/// Validate that `spec.listen_port` is free for this session: not already used
/// by this session (excluding `spec.id` itself, so a stopped rule can be
/// re-started) and not taken by another live session to the same endpoint.
/// Shared by `start_local_forward` and `restart_dead_forwards` (auto-restore),
/// so a second tab to the same host reports a clean conflict instead of
/// double-binding the port.
fn validate_listen_conflict(
    state: &AppState,
    rt: &SessionRuntime,
    spec: &ForwardSpec,
) -> Result<(), AppError> {
    let sid = rt.id.clone();
    {
        let g = rt
            .local_forwards
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dup = g.desired.iter().any(|s| {
            s.id != spec.id && s.kind == spec.kind && s.listen_port == spec.listen_port
        });
        let live_dup = g.live.values().any(|l| {
            l.spec.id != spec.id
                && l.spec.kind == spec.kind
                && l.spec.listen_port == spec.listen_port
                && l.handle.alive.load(Ordering::SeqCst)
        });
        if dup || live_dup {
            return Err(AppError::Message(match spec.kind {
                ForwardKind::Local => {
                    format!("本机端口 {} 已在本会话转发中", spec.listen_port)
                }
                ForwardKind::Remote => {
                    format!("远端端口 {} 已在本会话转发中", spec.listen_port)
                }
            }));
        }
    }
    match spec.kind {
        ForwardKind::Local => {
            if local_listen_taken_elsewhere(state, spec.listen_port, &sid) {
                return Err(AppError::Message(format!(
                    "本机端口 {} 已被其他会话占用",
                    spec.listen_port
                )));
            }
        }
        ForwardKind::Remote => {
            let cached = rt.cached.lock().expect("cached lock").clone();
            if let Some(c) = cached {
                if remote_listen_taken_elsewhere(state, &sid, &c.host, c.port, spec.listen_port) {
                    return Err(AppError::Message(format!(
                        "远端端口 {} 已在同一主机的其他会话转发中",
                        spec.listen_port
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Permanent (non-retryable) failures detected from a classified error message.
fn forward_failure_is_permanent(msg: &str) -> bool {
    let l = msg.to_ascii_lowercase();
    l.contains("address already in use")
        || l.contains("permission denied")
        || l.contains("已被占用")
        || l.contains("已在本会话")
        || l.contains("已被其他会话")
        || l.contains("allowtcpforwarding")
        || l.contains("gatewayports")
        || l.contains("被拒绝")
        || l.contains("无法监听")
        || l.contains("ssh-agent")
        || l.contains("agent only")
        || l.contains("administratively prohibited")
}

async fn start_port_forward_inner(
    app: &AppHandle,
    state: &AppState,
    req: StartPortForwardRequest,
) -> Result<ForwardInfo, AppError> {
    let sid = req.session_id.trim();
    if sid.is_empty() {
        return Err(AppError::Message("session_id 不能为空".into()));
    }
    let rt = state.get_runtime(sid)?;

    // Reuse a stopped rule's id when the same kind+listen_port is requested
    // again (lets the list's "启动" button / a re-create with the same port
    // reuse the persisted id instead of tripping the "already in session" check).
    let existing_id = {
        let g = rt
            .local_forwards
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        g.desired
            .iter()
            .find(|s| {
                s.kind == req.kind
                    && s.listen_port == req.listen_port
                    && !g.live.contains_key(&s.id)
            })
            .map(|s| s.id.clone())
    };

    let mut spec = parse_spec(
        req.kind,
        req.bind_address.as_deref(),
        req.listen_port,
        &req.dest_host,
        req.dest_port,
    )?;
    if let Some(id) = existing_id {
        spec.id = id;
    }

    {
        let n = rt
            .local_forwards
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .desired
            .len();
        if n >= MAX_FORWARDS_PER_SESSION {
            return Err(AppError::Message(format!(
                "每个会话最多 {MAX_FORWARDS_PER_SESSION} 条端口转发"
            )));
        }
    }

    validate_listen_conflict(state, &rt, &spec)?;

    // User explicitly (re)starting this rule: allow retries again.
    rt.local_forwards
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .permanent_failed
        .remove(&spec.id);

    spawn_and_attach(app, &rt, spec).await
}

#[tauri::command]
pub async fn stop_local_forward(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    forward_id: String,
) -> Result<(), String> {
    let rt = state
        .get_runtime(session_id.trim())
        .map_err(|e| -> String { e.into() })?;
    let id = forward_id.trim();
    if id.is_empty() {
        return Err(AppError::Message("forward_id 不能为空".into()).into());
    }
    {
        let mut g = rt
            .local_forwards
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        g.desired.retain(|s| s.id != id);
        g.last_error.remove(id);
        if let Some(live) = g.live.remove(id) {
            live.handle.start_kill();
        }
    }
    // Stop = forget the saved rule (so it won't auto-restore next time).
    if let Some(c) = rt.cached.lock().expect("cached lock").clone() {
        if !c.host.is_empty() && !c.username.is_empty() {
            crate::config::remove_rule(&c.host, c.port, &c.username, id);
        }
    }
    crate::ops_log::log(
        "SSH",
        &format!(
            "port_forward stop sid={} id={}",
            &rt.id[..rt.id.len().min(8)],
            &id[..id.len().min(8)]
        ),
    );
    emit_forwards(&app, &rt);
    Ok(())
}

#[tauri::command]
pub async fn list_local_forwards(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<ForwardInfo>, String> {
    let rt = state
        .get_runtime(session_id.trim())
        .map_err(|e| -> String { e.into() })?;
    Ok(rt.local_forwards.snapshot())
}

fn endpoint_of(rt: &SessionRuntime) -> Option<(String, u16, String)> {
    rt.cached
        .lock()
        .ok()
        .and_then(|c| c.clone())
        .filter(|c| !c.host.is_empty() && !c.username.is_empty())
        .map(|c| (c.host, c.port, c.username))
}

#[tauri::command]
pub async fn get_forward_auto_restore(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<bool, String> {
    let rt = state
        .get_runtime(session_id.trim())
        .map_err(|e| -> String { e.into() })?;
    match endpoint_of(&rt) {
        Some((host, port, user)) => Ok(crate::config::get_auto_restore(&host, port, &user)),
        None => Ok(false),
    }
}

#[derive(Debug, Deserialize)]
pub struct SetAutoRestoreRequest {
    pub session_id: String,
    pub enabled: bool,
}

#[tauri::command]
pub async fn set_forward_auto_restore(
    app: AppHandle,
    state: State<'_, AppState>,
    req: SetAutoRestoreRequest,
) -> Result<(), String> {
    let rt = state
        .get_runtime(req.session_id.trim())
        .map_err(|e| -> String { e.into() })?;
    if let Some((host, port, user)) = endpoint_of(&rt) {
        crate::config::set_auto_restore(&host, port, &user, req.enabled);
    }
    emit_forwards(&app, &rt);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_bind_and_formats_l_arg() {
        let s = parse_spec(ForwardKind::Local, None, 6379, "10.0.0.12", 6379).unwrap();
        assert_eq!(s.bind_address, "127.0.0.1");
        assert_eq!(s.kind, ForwardKind::Local);
        assert_eq!(s.ssh_arg(), "6379:10.0.0.12:6379");
        assert!(s.display().contains("-L"));
    }

    #[test]
    fn parse_remote_defaults_dest_localhost() {
        let s = parse_spec(ForwardKind::Remote, None, 8080, "", 3000).unwrap();
        assert_eq!(s.kind, ForwardKind::Remote);
        assert_eq!(s.bind_address, "127.0.0.1");
        assert_eq!(s.dest_host, "127.0.0.1");
        assert_eq!(s.listen_port, 8080);
        assert_eq!(s.dest_port, 3000);
        assert_eq!(s.ssh_arg(), "8080:127.0.0.1:3000");
        assert!(s.display().contains("-R"));
    }

    #[test]
    fn parse_trims_and_custom_bind() {
        let s = parse_spec(
            ForwardKind::Local,
            Some(" localhost "),
            8080,
            " 127.0.0.1 ",
            80,
        )
        .unwrap();
        assert_eq!(s.bind_address, "localhost");
        assert_eq!(s.dest_host, "127.0.0.1");
        assert_eq!(s.ssh_arg(), "8080:127.0.0.1:80");
        let all = parse_spec(
            ForwardKind::Local,
            Some("0.0.0.0"),
            8080,
            "10.0.0.1",
            80,
        )
        .unwrap();
        assert_eq!(all.ssh_arg(), "0.0.0.0:8080:10.0.0.1:80");
    }

    #[test]
    fn empty_dest_defaults_to_loopback() {
        let s = parse_spec(ForwardKind::Local, None, 6379, "  ", 6379).unwrap();
        assert_eq!(s.dest_host, "127.0.0.1");
        let r = parse_spec(ForwardKind::Remote, None, 8080, "", 3000).unwrap();
        assert_eq!(r.dest_host, "127.0.0.1");
    }

    #[test]
    fn reject_colon_in_host() {
        assert!(parse_spec(ForwardKind::Local, None, 1, "fe80::1", 1).is_err());
        assert!(parse_spec(ForwardKind::Remote, Some("::1"), 1, "a", 1).is_err());
    }

    #[test]
    fn reject_whitespace_and_slash() {
        assert!(parse_spec(ForwardKind::Local, None, 22, "host name", 22).is_err());
        assert!(parse_spec(ForwardKind::Local, None, 22, "a/b", 22).is_err());
    }

    #[test]
    fn reject_port_zero() {
        assert!(parse_spec(ForwardKind::Local, None, 0, "10.0.0.1", 6379).is_err());
        assert!(parse_spec(ForwardKind::Remote, None, 8080, "127.0.0.1", 0).is_err());
    }
}
