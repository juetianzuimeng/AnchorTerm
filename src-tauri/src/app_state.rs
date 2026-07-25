//! Application multi-session state.
//!
//! PR2: arbitrary client-generated `session_id` keys; no default shim.
//! Backend has **no** UI focus / current_session concept.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::auth::AuthMethod;
use crate::cwd::CwdTracker;
use crate::error::AppError;
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

/// Event payload for `session://error`.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorEvent {
    pub session_id: String,
    pub message: String,
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
    pub cols: AtomicU32,
    pub rows: AtomicU32,
    /// Debounce remote `stty` injects (per session).
    pub last_stty: Mutex<Option<Instant>>,
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
            cols: AtomicU32::new(80),
            rows: AtomicU32::new(24),
            last_stty: Mutex::new(None),
        }
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

    pub fn session_count(&self) -> usize {
        self.sessions.lock().expect("sessions lock").len()
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
        assert_eq!(app.session_count(), 0);
        let a = app.get_or_insert_runtime("s1").expect("insert");
        let b = app.get_or_insert_runtime("s1").expect("get");
        assert_eq!(a.id, b.id);
        assert_eq!(app.session_count(), 1);
        assert!(app.get_runtime("missing").is_err());
        app.remove_runtime("s1");
        assert_eq!(app.session_count(), 0);
        assert!(app.get_runtime("s1").is_err());
    }

    #[test]
    fn disconnect_keeps_map_entry_transport_none() {
        // Lifecycle semantics without real transport: entry exists, transport is None.
        let app = AppState::default();
        let rt = app.get_or_insert_runtime("s-keep").unwrap();
        assert!(rt.transport.lock().unwrap().is_none());
        assert_eq!(app.session_count(), 1);
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
}
