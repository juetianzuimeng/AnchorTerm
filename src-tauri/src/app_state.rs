//! Application multi-session state.
//!
//! PR2: arbitrary client-generated `session_id` keys; no default shim.
//! Backend has **no** UI focus / current_session concept.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::auth::AuthMethod;
use crate::cwd::CwdTracker;
use crate::error::AppError;
use crate::ssh::openssh::SecureKeyMaterial;
use crate::ssh::transport::ActiveTransport;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Connecting,
    Connected,
    /// Unexpected drop; auto-reconnect in progress.
    Reconnecting,
    Disconnected,
    Failed,
}

impl Default for SessionState {
    fn default() -> Self {
        SessionState::Idle
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub state: SessionState,
    pub host: Option<String>,
    pub username: Option<String>,
    pub message: Option<String>,
    pub cwd: Option<String>,
    pub attempt: Option<u32>,
}

/// Event payload for `session://data` (PR2 object shape).
#[derive(Debug, Clone, Serialize)]
pub struct DataEvent {
    pub session_id: String,
    pub data_b64: String,
}

/// Event payload for `session://cwd`.
#[derive(Debug, Clone, Serialize)]
pub struct CwdEvent {
    pub session_id: String,
    pub cwd: String,
}

/// Cached connection parameters for auto-reconnect (kept in memory only).
#[derive(Clone)]
pub struct CachedConnect {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub profile_id: Option<String>,
}

pub struct SessionMeta {
    pub state: SessionState,
    pub host: Option<String>,
    pub username: Option<String>,
    pub message: Option<String>,
    pub attempt: u32,
}

impl Default for SessionMeta {
    fn default() -> Self {
        Self {
            state: SessionState::Idle,
            host: None,
            username: None,
            message: None,
            attempt: 0,
        }
    }
}

/// One interactive SSH session (one tab).
pub struct SessionRuntime {
    pub id: String,
    pub transport: Mutex<Option<ActiveTransport>>,
    pub meta: Mutex<SessionMeta>,
    pub cwd: Mutex<CwdTracker>,
    /// Frozen absolute path captured at unexpected disconnect; preferred for restore.
    pub restore_target: Mutex<Option<String>>,
    pub cached: Mutex<Option<CachedConnect>>,
    /// When false, unexpected drops will not auto-reconnect (manual disconnect).
    pub auto_reconnect: AtomicBool,
    /// Bumped to cancel in-flight reconnect loops.
    pub reconnect_gen: AtomicU64,
    /// While true, ignore OSC 7 cwd updates (prevents new shell $HOME from wiping restore target).
    pub cwd_freeze: AtomicBool,
    /// While true, do not emit remote bytes to the terminal UI.
    /// Used during auto-reconnect / user re-connect so OpenSSH timeouts and
    /// login MOTD/banner do not pollute scrollback; silent restore `cd` stays hidden.
    pub ui_mute: AtomicBool,
    pub cols: AtomicU32,
    pub rows: AtomicU32,
    /// Debounce remote `stty` injects (per session).
    pub last_stty: Mutex<Option<Instant>>,
    /// Last (cols, rows) successfully injected to the remote PTY via `stty`.
    /// Distinct from local `cols`/`rows` (set at connect before any inject).
    pub remote_stty: Mutex<Option<(u32, u32)>>,
    /// Strip our injected `stty …` line from the UI stream (remote line echo).
    pub echo_suppress: Mutex<Option<EchoSuppress>>,
    /// Decrypted temp key reused by side-channel `ssh` (Tab complete / pwd).
    /// Avoids re-running icacls on every Tab (which flashed black consoles).
    pub side_channel_key: Mutex<Option<SecureKeyMaterial>>,
}

/// Pending filter for silent control injects (currently `stty` resize).
pub struct EchoSuppress {
    /// Exact command text without trailing CR/LF (as typed to the shell).
    pub pattern: Vec<u8>,
    /// Carry buffer for matches that span read chunks.
    pub carry: Vec<u8>,
    pub until: Instant,
}

impl SessionRuntime {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            transport: Mutex::new(None),
            meta: Mutex::new(SessionMeta::default()),
            cwd: Mutex::new(CwdTracker::new()),
            restore_target: Mutex::new(None),
            cached: Mutex::new(None),
            auto_reconnect: AtomicBool::new(true),
            reconnect_gen: AtomicU64::new(0),
            cwd_freeze: AtomicBool::new(false),
            ui_mute: AtomicBool::new(false),
            cols: AtomicU32::new(80),
            rows: AtomicU32::new(24),
            last_stty: Mutex::new(None),
            remote_stty: Mutex::new(None),
            echo_suppress: Mutex::new(None),
            side_channel_key: Mutex::new(None),
        }
    }

    /// Drop cached side-channel key (new connect / auth change / close).
    pub fn clear_side_channel_key(&self) {
        if let Ok(mut g) = self.side_channel_key.lock() {
            if g.take().is_some() {
                crate::ops_log::log("SSH", "side-channel key cache cleared");
            }
        }
    }

    pub fn set_ui_mute(&self, mute: bool) {
        self.ui_mute.store(mute, Ordering::SeqCst);
        crate::ops_log::log(
            "SSH",
            if mute {
                "ui_mute on (suppress terminal stream)"
            } else {
                "ui_mute off"
            },
        );
    }

    pub fn is_ui_muted(&self) -> bool {
        self.ui_mute.load(Ordering::SeqCst)
    }

    /// Arm UI-stream filter before writing a silent `stty` inject.
    pub fn arm_stty_echo_suppress(&self, cols: u32, rows: u32) {
        let pattern = format!("stty cols {cols} rows {rows} 2>/dev/null").into_bytes();
        if let Ok(mut g) = self.echo_suppress.lock() {
            *g = Some(EchoSuppress {
                pattern,
                carry: Vec::new(),
                until: Instant::now() + Duration::from_secs(3),
            });
        }
    }

    /// Remove injected control-command echo from bytes headed to the terminal UI.
    pub fn filter_outgoing_echo(&self, data: &[u8]) -> Vec<u8> {
        let mut g = match self.echo_suppress.lock() {
            Ok(x) => x,
            Err(e) => e.into_inner(),
        };
        let Some(state) = g.as_mut() else {
            return data.to_vec();
        };
        if Instant::now() > state.until {
            *g = None;
            return data.to_vec();
        }
        state.carry.extend_from_slice(data);
        let out = strip_echo_pattern(&mut state.carry, &state.pattern);
        // Keep suppress armed for the full window so a second identical stty
        // echo in the same burst is also dropped.
        out
    }

    /// Snapshot for UI. Lock order: `meta` then `cwd`.
    /// **Never call while already holding `cwd` or `meta`** — `Mutex` is not reentrant.
    pub fn snapshot(&self) -> SessionSnapshot {
        let meta = self.meta.lock().expect("meta lock");
        let cwd = self
            .cwd
            .lock()
            .expect("cwd lock")
            .last_known()
            .map(|s| s.to_string());
        let attempt = if matches!(
            meta.state,
            SessionState::Reconnecting | SessionState::Connecting
        ) {
            Some(meta.attempt)
        } else {
            None
        };
        SessionSnapshot {
            session_id: self.id.clone(),
            state: meta.state.clone(),
            host: meta.host.clone(),
            username: meta.username.clone(),
            message: meta.message.clone(),
            cwd,
            attempt,
        }
    }

    pub fn set_term_size(&self, cols: u32, rows: u32) {
        self.cols.store(cols.max(20), Ordering::Relaxed);
        self.rows.store(rows.max(5), Ordering::Relaxed);
    }

    pub fn term_size(&self) -> (u32, u32) {
        (
            self.cols.load(Ordering::Relaxed).max(20),
            self.rows.load(Ordering::Relaxed).max(5),
        )
    }
}

/// Strip all occurrences of `pattern` plus following CR/LF from `carry`.
/// Incomplete suffix of `pattern` is retained in `carry` across chunks.
pub fn strip_echo_pattern(carry: &mut Vec<u8>, pattern: &[u8]) -> Vec<u8> {
    if pattern.is_empty() {
        return std::mem::take(carry);
    }
    let mut out = Vec::with_capacity(carry.len());
    loop {
        if let Some(pos) = find_slice(carry, pattern) {
            out.extend_from_slice(&carry[..pos]);
            let mut end = pos + pattern.len();
            while end < carry.len() && (carry[end] == b'\r' || carry[end] == b'\n') {
                end += 1;
            }
            let rest = carry[end..].to_vec();
            *carry = rest;
            crate::ops_log::log("SSH", "stty echo suppressed from UI stream");
        } else {
            // Keep only a *suffix* that is a prefix of `pattern` (possible incomplete match).
            let max_keep = pattern.len().saturating_sub(1).min(carry.len());
            let mut keep = 0;
            for k in (1..=max_keep).rev() {
                if carry.ends_with(&pattern[..k]) {
                    keep = k;
                    break;
                }
            }
            let emit_len = carry.len() - keep;
            out.extend_from_slice(&carry[..emit_len]);
            let rest = carry[emit_len..].to_vec();
            *carry = rest;
            break;
        }
    }
    out
}

fn find_slice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Process-wide app state: session map only (no UI focus).
pub struct AppState {
    pub sessions: Mutex<HashMap<String, Arc<SessionRuntime>>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

impl AppState {
    /// Look up a session by id.
    pub fn get_runtime(&self, session_id: &str) -> Result<Arc<SessionRuntime>, AppError> {
        let map = self.sessions.lock().expect("sessions lock");
        map.get(session_id)
            .map(Arc::clone)
            .ok_or_else(|| AppError::SessionNotFound(session_id.to_string()))
    }

    /// Insert a new runtime, or return existing if id already present.
    /// Call **before** any emit for this session_id.
    pub fn get_or_insert_runtime(&self, session_id: &str) -> Result<Arc<SessionRuntime>, AppError> {
        if session_id.is_empty() {
            return Err(AppError::Message("session_id 不能为空".into()));
        }
        let mut map = self.sessions.lock().expect("sessions lock");
        if let Some(rt) = map.get(session_id) {
            return Ok(Arc::clone(rt));
        }
        let rt = Arc::new(SessionRuntime::new(session_id));
        map.insert(session_id.to_string(), Arc::clone(&rt));
        Ok(rt)
    }

    /// Remove session from map (after transport taken / reconnect cancelled).
    pub fn remove_runtime(&self, session_id: &str) -> Option<Arc<SessionRuntime>> {
        let mut map = self.sessions.lock().expect("sessions lock");
        map.remove(session_id)
    }

    pub fn list_session_ids(&self) -> Vec<String> {
        let map = self.sessions.lock().expect("sessions lock");
        map.keys().cloned().collect()
    }

    pub fn list_snapshots(&self) -> Vec<SessionSnapshot> {
        let map = self.sessions.lock().expect("sessions lock");
        map.values().map(|rt| rt.snapshot()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn snapshot_includes_session_id() {
        let rt = SessionRuntime::new("abc-123");
        let snap = rt.snapshot();
        assert_eq!(snap.session_id, "abc-123");
        assert_eq!(snap.state, SessionState::Idle);
    }

    #[test]
    fn get_or_insert_and_remove() {
        let app = AppState::default();
        assert_eq!(app.list_session_ids().len(), 0);
        let a = app.get_or_insert_runtime("s1").expect("insert");
        let b = app.get_or_insert_runtime("s1").expect("get");
        assert_eq!(a.id, b.id);
        assert_eq!(app.list_session_ids().len(), 1);
        assert!(app.get_runtime("missing").is_err());
        app.remove_runtime("s1");
        assert_eq!(app.list_session_ids().len(), 0);
        assert!(app.get_runtime("s1").is_err());
    }

    #[test]
    fn disconnect_keeps_map_entry_transport_none() {
        // Lifecycle semantics without real transport: entry exists, transport is None.
        let app = AppState::default();
        let rt = app.get_or_insert_runtime("s-keep").unwrap();
        assert!(rt.transport.lock().unwrap().is_none());
        assert_eq!(app.list_session_ids().len(), 1);
        // close removes
        app.remove_runtime("s-keep");
        assert!(app.get_runtime("s-keep").is_err());
    }

    #[test]
    fn reconnect_gen_bump_isolates_loops() {
        let rt = SessionRuntime::new("s-gen");
        let g0 = rt.reconnect_gen.load(Ordering::SeqCst);
        let g1 = rt.reconnect_gen.fetch_add(1, Ordering::SeqCst) + 1;
        assert_ne!(g0, g1);
        assert_eq!(rt.reconnect_gen.load(Ordering::SeqCst), g1);
        // Old loop would see gen mismatch
        assert_ne!(rt.reconnect_gen.load(Ordering::SeqCst), g0);
    }

    #[test]
    fn last_stty_is_per_runtime() {
        let a = SessionRuntime::new("a");
        let b = SessionRuntime::new("b");
        *a.last_stty.lock().unwrap() = Some(Instant::now());
        assert!(b.last_stty.lock().unwrap().is_none());
        // B can still "fire" immediately; A is debounced if checked
        let a_recent = a
            .last_stty
            .lock()
            .unwrap()
            .map(|t| t.elapsed() < Duration::from_millis(800))
            .unwrap_or(false);
        assert!(a_recent);
    }

    #[test]
    fn strip_stty_echo_single_chunk() {
        let mut carry = b"hello\nstty cols 80 rows 24 2>/dev/null\r\nworld".to_vec();
        let pat = b"stty cols 80 rows 24 2>/dev/null";
        let out = strip_echo_pattern(&mut carry, pat);
        assert_eq!(String::from_utf8_lossy(&out), "hello\nworld");
        assert!(carry.is_empty());
    }

    #[test]
    fn strip_stty_echo_cross_chunk() {
        let pat = b"stty cols 100 rows 30 2>/dev/null";
        let mut carry = b"stty cols 100 ro".to_vec();
        let out1 = strip_echo_pattern(&mut carry, pat);
        assert!(out1.is_empty(), "incomplete pattern must not emit: {out1:?}");
        carry.extend_from_slice(b"ws 30 2>/dev/null\r\nok");
        let out2 = strip_echo_pattern(&mut carry, pat);
        assert_eq!(String::from_utf8_lossy(&out2), "ok");
    }

    #[test]
    fn filter_outgoing_echo_via_runtime() {
        let rt = SessionRuntime::new("s");
        rt.arm_stty_echo_suppress(120, 40);
        let filtered = rt.filter_outgoing_echo(b"stty cols 120 rows 40 2>/dev/null\r\nprompt$ ");
        assert_eq!(String::from_utf8_lossy(&filtered), "prompt$ ");
    }
}
