use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::auth::AuthMethod;
use crate::cwd::CwdTracker;
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
    pub state: SessionState,
    pub host: Option<String>,
    pub username: Option<String>,
    pub message: Option<String>,
    pub cwd: Option<String>,
    pub attempt: Option<u32>,
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

pub struct AppState {
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
}

impl Default for AppState {
    fn default() -> Self {
        Self {
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
        }
    }
}

impl AppState {
    /// Snapshot for UI. Lock order: `meta` then `cwd`.
    /// **Never call while already holding `cwd` or `meta`** — `Mutex` is not reentrant
    /// (that deadlock froze the SSH stdout pump after the first `cd` command).
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
