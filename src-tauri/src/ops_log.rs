//! Operation / diagnostic log written under the project `操作日志` directory.
//!
//! Records UI actions (via IPC), SSH lifecycle, stdin writes, and remote echo
//! so failures can be analyzed offline without a live debugger.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::info;

/// Fixed project-relative log root (as requested).
pub const OPS_LOG_DIR: &str = r"C:\zengshangchun\AnchorTerm\操作日志";

static STATE: Mutex<Option<LogState>> = Mutex::new(None);

struct LogState {
    session_path: PathBuf,
    daily_path: PathBuf,
    latest_path: PathBuf,
}

fn now_stamp() -> String {
    // Local-ish wall clock via UTC offset is optional; use Unix ms for ordering.
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    // YYYY-MM-DDTHH:MM:SS.mmmZ (UTC)
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    // Civil date from days since epoch (proleptic Gregorian)
    let (y, mo, day) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

/// Algorithm from Howard Hinnant civil_from_days (public domain).
fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    z += 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

fn file_day_stamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let days = (d.as_secs() / 86400) as i64;
    let (y, mo, day) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{day:02}")
}

fn file_session_stamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    let (y, mo, day) = civil_from_days(days as i64);
    format!("{y:04}{mo:02}{day:02}-{h:02}{m:02}{s:02}")
}

/// Ensure log dir exists and open session + daily + latest files.
///
/// On every app start, **all previous log files** under the ops dir are removed
/// so analysis only sees the current run (README.md is kept).
pub fn init() -> PathBuf {
    let dir = PathBuf::from(OPS_LOG_DIR);
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("ops_log: create_dir_all failed: {e}");
    }

    // Wipe prior run artifacts so each restart starts with a clean slate.
    clear_previous_logs(&dir);

    let session_path = dir.join(format!("session-{}.log", file_session_stamp()));
    let daily_path = dir.join(format!("ops-{}.log", file_day_stamp()));
    let latest_path = dir.join("latest.log");

    let header = format!(
        "===== AnchorTerm ops log start {} session={} =====\n",
        now_stamp(),
        session_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
    );
    append_raw(&session_path, &header);
    append_raw(&daily_path, &header);
    append_raw(&latest_path, &header);

    if let Ok(mut g) = STATE.lock() {
        *g = Some(LogState {
            session_path: session_path.clone(),
            daily_path,
            latest_path,
        });
    }

    info!(path = %session_path.display(), "ops_log initialized (previous logs cleared)");
    session_path
}

/// Delete all `*.log` files in the ops directory. Keep non-log files (e.g. README.md).
fn clear_previous_logs(dir: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ops_log: read_dir for clear failed: {e}");
            return;
        }
    };
    let mut removed = 0u32;
    for ent in entries.flatten() {
        let path = ent.path();
        let is_log = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("log"))
            .unwrap_or(false);
        if !is_log {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) => eprintln!("ops_log: remove {} failed: {e}", path.display()),
        }
    }
    // Also clear empty latest if recreate races; init will rewrite header.
    let _ = removed;
}

fn append_raw(path: &Path, text: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
        let _ = f.flush();
    }
}

/// Write one structured log line to all log files.
///
/// `category` examples: UI, SSH, ECHO, STATE, ERR, SYS
pub fn log(category: &str, message: &str) {
    let line = format!("{} [{}] {}\n", now_stamp(), category, message);
    // Always try to write even if init was skipped.
    let dir = PathBuf::from(OPS_LOG_DIR);
    let _ = fs::create_dir_all(&dir);

    if let Ok(g) = STATE.lock() {
        if let Some(ref st) = *g {
            append_raw(&st.session_path, &line);
            append_raw(&st.daily_path, &line);
            append_raw(&st.latest_path, &line);
            return;
        }
    }
    // Fallback before init
    append_raw(&dir.join("latest.log"), &line);
}

pub fn log_fmt(category: &str, args: std::fmt::Arguments<'_>) {
    log(category, &format!("{args}"));
}

/// Hex preview of bytes (truncated).
pub fn hex_preview(data: &[u8], max: usize) -> String {
    let n = data.len().min(max);
    let mut s = String::with_capacity(n * 2 + 8);
    for b in &data[..n] {
        s.push_str(&format!("{b:02x}"));
    }
    if data.len() > max {
        s.push_str("…");
    }
    s
}

/// Safe UTF-8 preview for logs (escape control chars).
pub fn text_preview(data: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(data);
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        match ch {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Redact secrets in free-form strings (best-effort).
pub fn redact(s: &str) -> String {
    // Never log full private key paths with "passphrase" values; caller should not pass them.
    s.replace("password=", "password=[REDACTED]")
        .replace("passphrase=", "passphrase=[REDACTED]")
}

// --- Tauri command ---

#[derive(Debug, Serialize)]
pub struct OpsLogInfo {
    pub dir: String,
    pub latest: String,
    pub session: Option<String>,
}

#[tauri::command]
pub fn ops_log(
    category: String,
    message: String,
    detail: Option<String>,
) -> Result<(), String> {
    let cat = if category.is_empty() {
        "UI".to_string()
    } else {
        category
    };
    let msg = if let Some(d) = detail {
        if d.is_empty() {
            message
        } else {
            format!("{message} | {d}")
        }
    } else {
        message
    };
    // Frontend must never send secrets; still redact common patterns.
    log(&cat, &redact(&msg));
    Ok(())
}

#[tauri::command]
pub fn ops_log_info() -> Result<OpsLogInfo, String> {
    let dir = PathBuf::from(OPS_LOG_DIR);
    let latest = dir.join("latest.log");
    let session = STATE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.session_path.display().to_string()));
    Ok(OpsLogInfo {
        dir: dir.display().to_string(),
        latest: latest.display().to_string(),
        session,
    })
}

/// Convenience macros-like helpers used from Rust modules.
#[macro_export]
macro_rules! ops {
    ($cat:expr, $($arg:tt)*) => {{
        $crate::ops_log::log($cat, &format!($($arg)*));
    }};
}
