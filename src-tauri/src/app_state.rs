//! Application multi-session state.
//!
//! PR2: arbitrary client-generated `session_id` keys; no default shim.
//! Backend has **no** UI focus / current_session concept.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::auth::AuthMethod;
use crate::cwd::CwdTracker;
use crate::error::AppError;
use crate::output_ring::OutputRing;
use crate::ssh::complete_cache::SessionCompleteCache;
use crate::ssh::forward::ForwardSet;
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
    /// True while `reconnect_loop` is running. Prevents `finish_session` from
    /// spawning a second loop (which reset attempt to 1 and painted a stale
    /// 「重连失败 (1)」 over the live retry).
    pub reconnect_in_flight: AtomicBool,
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
    /// Bumped when the session drops so an in-flight restore playbook aborts
    /// without painting a false "Connected" over a dead transport.
    pub restore_gen: AtomicU64,
    /// Rate-limit ECHO ops-log lines (huge `grep`/`tail` floods freezes UI).
    pub echo_log_budget: AtomicU32,
    /// OpenSSH ControlPath for ControlMaster multiplexing (interactive + Tab complete).
    pub control_path: Mutex<Option<PathBuf>>,
    /// Hybrid Tab completion directory/command cache (per session).
    pub complete_cache: Mutex<SessionCompleteCache>,
    // --- post-command separator diagnostics (SEP category) ---
    /// True while we expect an OSC end-marker for the last post-sep submit.
    pub sep_pending: AtomicBool,
    /// Monotonic id of the last post-sep submit (for correlating SEP logs).
    pub sep_gen: AtomicU64,
    /// Chunks received on ssh stdout since last post-sep submit.
    pub sep_chunks: AtomicU64,
    /// Bytes received on ssh stdout since last post-sep submit (raw, pre-filter).
    pub sep_bytes_in: AtomicU64,
    /// Bytes emitted to UI since last post-sep submit (post-filter).
    pub sep_bytes_out: AtomicU64,
    /// Chunks dropped because filter produced empty output (while sep pending).
    pub sep_empty_drops: AtomicU64,
    /// Last time on_data processed a chunk (ms since UNIX_EPOCH); 0 = never.
    pub sep_last_on_data_ms: AtomicU64,
    /// Recent UI-visible terminal output for MCP `session_read_output` (PR-M4).
    pub output_ring: Mutex<OutputRing>,
    /// Concurrent MCP side-channel execs on this session.
    pub mcp_exec_inflight: AtomicU32,
    /// TCP forwards (`ssh -N -L` / `-R`) owned by this tab.
    pub local_forwards: ForwardSet,
}

/// Pending filter for silent control injects (`stty` resize, post-cmd marker echo).
pub struct EchoSuppress {
    /// Exact command text without trailing CR/LF (as typed to the shell).
    pub pattern: Vec<u8>,
    /// Carry buffer for matches that span read chunks.
    pub carry: Vec<u8>,
    pub until: Instant,
    /// When true, clear suppress immediately after the first successful strip
    /// (post-cmd separator marker). When false, keep armed until `until`
    /// (stty may echo twice in one burst).
    pub once: bool,
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
            reconnect_in_flight: AtomicBool::new(false),
            cwd_freeze: AtomicBool::new(false),
            ui_mute: AtomicBool::new(false),
            cols: AtomicU32::new(80),
            rows: AtomicU32::new(24),
            last_stty: Mutex::new(None),
            remote_stty: Mutex::new(None),
            echo_suppress: Mutex::new(None),
            side_channel_key: Mutex::new(None),
            restore_gen: AtomicU64::new(0),
            // Few bulk ECHO samples per connection (refilled rarely).
            echo_log_budget: AtomicU32::new(8),
            control_path: Mutex::new(None),
            complete_cache: Mutex::new(SessionCompleteCache::default()),
            sep_pending: AtomicBool::new(false),
            sep_gen: AtomicU64::new(0),
            sep_chunks: AtomicU64::new(0),
            sep_bytes_in: AtomicU64::new(0),
            sep_bytes_out: AtomicU64::new(0),
            sep_empty_drops: AtomicU64::new(0),
            sep_last_on_data_ms: AtomicU64::new(0),
            output_ring: Mutex::new(OutputRing::default()),
            mcp_exec_inflight: AtomicU32::new(0),
            local_forwards: ForwardSet::default(),
        }
    }

    /// Begin tracking a post-command-separator submit (resets counters).
    pub fn sep_begin(&self, logical: &str, payload_len: usize) {
        let gen = self.sep_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.sep_pending.store(true, Ordering::SeqCst);
        self.sep_chunks.store(0, Ordering::SeqCst);
        self.sep_bytes_in.store(0, Ordering::SeqCst);
        self.sep_bytes_out.store(0, Ordering::SeqCst);
        self.sep_empty_drops.store(0, Ordering::SeqCst);
        crate::ops_log::log(
            "SEP",
            &format!(
                "begin gen={} sid={} line=\"{}\" payload_len={} suppress_armed=1",
                gen,
                &self.id[..self.id.len().min(8)],
                crate::ops_log::text_preview(logical.as_bytes(), 120),
                payload_len
            ),
        );
    }

    /// Snapshot counters for a SEP progress / end line.
    /// `suppress_known`: `Some(true/false)` when caller already knows arm state
    /// (avoids re-locking `echo_suppress` while holding it).
    pub fn sep_stats_line(&self, stage: &str, suppress_known: Option<bool>) -> String {
        let suppress = suppress_known.unwrap_or_else(|| {
            self.echo_suppress
                .lock()
                .map(|g| g.is_some())
                .unwrap_or(false)
        });
        format!(
            "{} gen={} sid={} pending={} chunks={} bytes_in={} bytes_out={} empty_drops={} last_on_data_ms={} suppress={}",
            stage,
            self.sep_gen.load(Ordering::Relaxed),
            &self.id[..self.id.len().min(8)],
            self.sep_pending.load(Ordering::Relaxed) as u8,
            self.sep_chunks.load(Ordering::Relaxed),
            self.sep_bytes_in.load(Ordering::Relaxed),
            self.sep_bytes_out.load(Ordering::Relaxed),
            self.sep_empty_drops.load(Ordering::Relaxed),
            self.sep_last_on_data_ms.load(Ordering::Relaxed),
            suppress as u8
        )
    }

    pub fn sep_note_marker_injected(&self) {
        self.sep_pending.store(false, Ordering::SeqCst);
        crate::ops_log::log(
            "SEP",
            &self.sep_stats_line("marker_injected_done", Some(false)),
        );
    }

    /// Stable ControlPath for this tab (created once). Used by ControlMaster.
    pub fn ensure_control_path(&self) -> PathBuf {
        let mut g = self
            .control_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(ref p) = *g {
            return p.clone();
        }
        let safe: String = self
            .id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(36)
            .collect();
        let p = std::env::temp_dir().join(format!("anchorterm-cm-{safe}"));
        *g = Some(p.clone());
        crate::ops_log::log(
            "SSH",
            &format!(
                "control_path set sid={} path={}",
                &self.id[..self.id.len().min(8)],
                p.display()
            ),
        );
        p
    }

    pub fn control_path_opt(&self) -> Option<PathBuf> {
        self.control_path
            .lock()
            .ok()
            .and_then(|g| g.clone())
    }

    /// Drop cached side-channel key (new connect / auth change / close).
    pub fn clear_side_channel_key(&self) {
        if let Ok(mut g) = self.side_channel_key.lock() {
            if g.take().is_some() {
                crate::ops_log::log("SSH", "side-channel key cache cleared");
            }
        }
    }

    /// Invalidate any restore playbook still sleeping / side-channel waiting.
    pub fn bump_restore_gen(&self) -> u64 {
        self.restore_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// True if this restore generation is still current.
    pub fn restore_gen_matches(&self, gen: u64) -> bool {
        self.restore_gen.load(Ordering::SeqCst) == gen
    }

    /// Allow a few ECHO log lines then silence bulk output until refilled.
    #[allow(dead_code)]
    pub fn take_echo_log_slot(&self) -> bool {
        loop {
            let cur = self.echo_log_budget.load(Ordering::Relaxed);
            if cur == 0 {
                return false;
            }
            if self
                .echo_log_budget
                .compare_exchange_weak(cur, cur - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    pub fn refill_echo_log_budget(&self) {
        self.echo_log_budget.store(8, Ordering::Relaxed);
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

    /// Arm UI-stream filter for an exact injected/typed pattern (line echo).
    pub fn arm_echo_suppress_pattern(&self, pattern: Vec<u8>, ttl: Duration, once: bool) {
        if pattern.is_empty() {
            return;
        }
        // Only log SEP for post-cmd (once) arms; stty arms are routine noise.
        if once {
            crate::ops_log::log(
                "SEP",
                &format!(
                    "suppress_arm sid={} once=1 ttl_ms={} pat_len={} pat=\"{}\"",
                    &self.id[..self.id.len().min(8)],
                    ttl.as_millis(),
                    pattern.len(),
                    crate::ops_log::text_preview(&pattern, 80)
                ),
            );
        }
        if let Ok(mut g) = self.echo_suppress.lock() {
            *g = Some(EchoSuppress {
                pattern,
                carry: Vec::new(),
                until: Instant::now() + ttl,
                once,
            });
        }
    }

    /// Arm UI-stream filter before writing a silent `stty` inject.
    pub fn arm_stty_echo_suppress(&self, cols: u32, rows: u32) {
        let pattern = format!("stty cols {cols} rows {rows} 2>/dev/null").into_bytes();
        // Keep armed for the full window: a second identical stty may echo in-burst.
        // once=false; do not use SEP category spam for stty — still logs via suppress_arm.
        self.arm_echo_suppress_pattern(pattern, Duration::from_secs(3), false);
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
            // Flush any held residual so we never drop bytes on TTL expiry.
            let carry_len = state.carry.len();
            let once = state.once;
            let mut leftover = std::mem::take(&mut state.carry);
            *g = None;
            drop(g); // release before logging (sep_stats may lock)
            leftover.extend_from_slice(data);
            crate::ops_log::log(
                "SEP",
                &format!(
                    "suppress_ttl_flush sid={} once={} carry_was={} in={} out={} {}",
                    &self.id[..self.id.len().min(8)],
                    once as u8,
                    carry_len,
                    data.len(),
                    leftover.len(),
                    self.sep_stats_line("after_ttl", Some(false))
                ),
            );
            return leftover;
        }
        let once = state.once;
        let carry_before = state.carry.len();
        state.carry.extend_from_slice(data);
        let matched = find_slice(&state.carry, &state.pattern).is_some();
        let mut out = strip_echo_pattern(&mut state.carry, &state.pattern);
        let carry_after = state.carry.len();
        // Post-cmd marker: disarm immediately after the line-echo is stripped so
        // large command output (tail -1000 …) is never scanned by this filter.
        if once && matched {
            out.extend_from_slice(&state.carry);
            state.carry.clear();
            *g = None;
            drop(g);
            crate::ops_log::log(
                "SEP",
                &format!(
                    "suppress_match_disarm sid={} in={} out={} carry_before={} carry_after_strip={} {}",
                    &self.id[..self.id.len().min(8)],
                    data.len(),
                    out.len(),
                    carry_before,
                    carry_after,
                    self.sep_stats_line("after_disarm", Some(false))
                ),
            );
            return out;
        }
        // Active suppress without match: sample log when holding residual or shrinking output.
        if once && (out.len() != data.len() || carry_after > 0) {
            let stats = self.sep_stats_line("pass", Some(true));
            // still holding g — do not call anything that re-locks echo_suppress
            crate::ops_log::log(
                "SEP",
                &format!(
                    "suppress_pass sid={} matched=0 in={} out={} carry_before={} carry_after={} {}",
                    &self.id[..self.id.len().min(8)],
                    data.len(),
                    out.len(),
                    carry_before,
                    carry_after,
                    stats
                ),
            );
        }
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

    /// Interactive PTY is usable: transport present **and** OpenSSH child still alive.
    ///
    /// UI `SessionState::Connected` can briefly lag a dead child (or restore can
    /// repaint Connected after transport was cleared). Prefer this for writes.
    pub fn pty_is_live(&self) -> bool {
        self.transport
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|t| t.is_alive()))
            .unwrap_or(false)
    }
}

/// Strip all occurrences of `pattern` plus following CR/LF from `carry`.
/// Incomplete suffix of `pattern` is retained in `carry` across chunks.
///
/// - **Whole-line** match (pattern at start of buffer or after CR/LF): drop the
///   line including its trailing EOL (silent `stty` inject).
/// - **Mid-line suffix** match (e.g. post-command separator chained after the
///   user command): drop only the pattern and re-insert `\r\n` so the remaining
///   command text still ends a line and does not glue to following output.
pub fn strip_echo_pattern(carry: &mut Vec<u8>, pattern: &[u8]) -> Vec<u8> {
    if pattern.is_empty() {
        return std::mem::take(carry);
    }
    let mut out = Vec::with_capacity(carry.len());
    loop {
        if let Some(pos) = find_slice(carry, pattern) {
            out.extend_from_slice(&carry[..pos]);
            let mut end = pos + pattern.len();
            let had_eol =
                end < carry.len() && (carry[end] == b'\r' || carry[end] == b'\n');
            while end < carry.len() && (carry[end] == b'\r' || carry[end] == b'\n') {
                end += 1;
            }
            let at_line_start =
                pos == 0 || carry[pos - 1] == b'\n' || carry[pos - 1] == b'\r';
            if !at_line_start && had_eol {
                out.extend_from_slice(b"\r\n");
            }
            let rest = carry[end..].to_vec();
            *carry = rest;
            crate::ops_log::log("SSH", "echo pattern suppressed from UI stream");
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

/// Process-wide app state: session map + MCP runtime (no UI focus).
pub struct AppState {
    pub sessions: Mutex<HashMap<String, Arc<SessionRuntime>>>,
    /// MCP server config + optional localhost listener (PR-M1).
    pub mcp: Mutex<crate::mcp::McpRuntime>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            mcp: Mutex::new(crate::mcp::McpRuntime::default()),
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

    #[test]
    fn strip_midline_suffix_keeps_eol() {
        // Post-command separator is chained after the user command; stripping it
        // must leave the user command on its own line.
        let suffix = b";printf 'x'";
        let mut carry = b"tail -200 test.log;printf 'x'\r\nlogline\n".to_vec();
        let out = strip_echo_pattern(&mut carry, suffix);
        assert_eq!(
            String::from_utf8_lossy(&out),
            "tail -200 test.log\r\nlogline\n"
        );
        assert!(carry.is_empty());
    }

    #[test]
    fn once_suppress_disarms_after_match_so_large_output_passes() {
        let rt = SessionRuntime::new("s");
        // Same-line short bash $'...' suffix (matches session::POST_CMD_SEP_SUFFIX).
        let suffix = b";printf $'\\e]733;ATsep\\a'";
        rt.arm_echo_suppress_pattern(suffix.to_vec(), Duration::from_secs(5), true);

        // Typed line echo with suffix — stripped, suppress disarmed.
        let echo = b"ls;printf $'\\e]733;ATsep\\a'\r\n";
        let out1 = rt.filter_outgoing_echo(echo);
        assert_eq!(String::from_utf8_lossy(&out1), "ls\r\n");
        assert!(rt.echo_suppress.lock().unwrap().is_none());

        // Subsequent large chunk must pass through untouched (no filter).
        let big = vec![b'x'; 8192];
        let out2 = rt.filter_outgoing_echo(&big);
        assert_eq!(out2, big);
    }

    #[test]
    fn suppress_ttl_flush_does_not_drop_carry() {
        let rt = SessionRuntime::new("s");
        // Pattern that will not appear; residual prefix held in carry.
        rt.arm_echo_suppress_pattern(b"ZZZNOMATCH".to_vec(), Duration::from_millis(1), true);
        let partial = rt.filter_outgoing_echo(b"helloZZ");
        // "ZZ" may be held as prefix of ZZZNOMATCH — either emitted or in carry.
        std::thread::sleep(Duration::from_millis(5));
        let rest = rt.filter_outgoing_echo(b"world");
        let combined = [partial.as_slice(), rest.as_slice()].concat();
        assert!(
            combined.windows(5).any(|w| w == b"hello") || combined.windows(5).any(|w| w == b"world"),
            "got {:?}",
            String::from_utf8_lossy(&combined)
        );
        // All user bytes should eventually appear.
        let text = String::from_utf8_lossy(&combined);
        assert!(text.contains('h') || text.contains('w'));
        // After TTL, suppress is gone and no residual permanently lost: helloworld or hello + world
        assert!(
            text.contains("hello") && text.contains("world"),
            "lost residual on TTL: {text:?}"
        );
    }
}
