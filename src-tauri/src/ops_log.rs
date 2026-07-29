//! Operation / diagnostic log directory resolution and writers.
//!
//! Path priority (DEPLOYMENT §5.2):
//! 1. Env `ANCHORTERM_LOG_DIR` if set and non-empty
//! 2. Repo tree `操作日志\` when running from a source checkout (exe under target/…)
//! 3. Otherwise installed/portable → `%APPDATA%\AnchorTerm\logs\`
//!
//! Each app start clears `*.log` in the resolved directory (README and other
//! non-log files are kept). Never log password / passphrase plaintext.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::info;

/// How the active log directory was chosen (for diagnostics / UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogDirSource {
    Env,
    DevTree,
    AppData,
    Fallback,
}

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
static LOG_SOURCE: OnceLock<LogDirSource> = OnceLock::new();
static STATE: Mutex<Option<LogState>> = Mutex::new(None);

struct LogState {
    session_path: PathBuf,
    daily_path: PathBuf,
    latest_path: PathBuf,
}

/// Resolved ops-log directory (initialized on first access / `init`).
pub fn log_dir() -> PathBuf {
    ensure_resolved().0.clone()
}

pub fn log_dir_source() -> LogDirSource {
    ensure_resolved().1
}

fn ensure_resolved() -> (&'static PathBuf, LogDirSource) {
    let dir = LOG_DIR.get_or_init(resolve_log_dir);
    let source = *LOG_SOURCE.get().unwrap_or(&LogDirSource::Fallback);
    (dir, source)
}

/// Resolve log directory without mutating globals (used by tests).
pub fn resolve_log_dir_with(
    env_override: Option<&str>,
    current_exe: Option<&Path>,
) -> (PathBuf, LogDirSource) {
    if let Some(raw) = env_override {
        let t = raw.trim();
        if !t.is_empty() {
            return (PathBuf::from(t), LogDirSource::Env);
        }
    }

    if let Some(exe) = current_exe {
        if let Some(dev) = find_repo_ops_log_dir(exe) {
            return (dev, LogDirSource::DevTree);
        }
    }

    if let Some(base) = dirs::config_dir() {
        return (base.join("AnchorTerm").join("logs"), LogDirSource::AppData);
    }

    // Last resort: cwd-relative (should be rare).
    (
        PathBuf::from("AnchorTerm-logs"),
        LogDirSource::Fallback,
    )
}

fn resolve_log_dir() -> PathBuf {
    let env = std::env::var("ANCHORTERM_LOG_DIR").ok();
    let exe = std::env::current_exe().ok();
    let (dir, source) = resolve_log_dir_with(
        env.as_deref(),
        exe.as_deref(),
    );
    let _ = LOG_SOURCE.set(source);
    dir
}

/// Walk up from the executable looking for the AnchorTerm repo root
/// (`package.json` + `src-tauri/`), then use `<root>/操作日志`.
fn find_repo_ops_log_dir(exe: &Path) -> Option<PathBuf> {
    let mut dir = exe.parent()?.to_path_buf();
    for _ in 0..10 {
        if is_repo_root(&dir) {
            return Some(dir.join("操作日志"));
        }
        // Common layout: …/src-tauri/target/{debug,release}/anchorterm.exe
        if dir.file_name().and_then(|s| s.to_str()) == Some("src-tauri") {
            if let Some(parent) = dir.parent() {
                if is_repo_root(parent) {
                    return Some(parent.join("操作日志"));
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn is_repo_root(dir: &Path) -> bool {
    dir.join("package.json").is_file() && dir.join("src-tauri").is_dir()
}

fn now_stamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
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
    let dir = log_dir();
    let source = log_dir_source();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("ops_log: create_dir_all failed: {e}");
    }

    clear_previous_logs(&dir);

    let session_path = dir.join(format!("session-{}.log", file_session_stamp()));
    let daily_path = dir.join(format!("ops-{}.log", file_day_stamp()));
    let latest_path = dir.join("latest.log");

    let header = format!(
        "===== AnchorTerm ops log start {} source={:?} session={} =====\n",
        now_stamp(),
        source,
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

    info!(
        path = %session_path.display(),
        ?source,
        "ops_log initialized (previous logs cleared)"
    );
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
    let _ = removed;
}

fn append_raw(path: &Path, text: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
        // Intentionally no flush(): triple-file fsync on every ECHO chunk can
        // stall the SSH stdout pump during large `tail`/`grep` floods.
        // Page cache is enough for diagnostics; process exit still persists.
    }
}

/// Write one structured log line to all log files.
///
/// `category` examples: UI, SSH, ECHO, STATE, ERR, SYS
pub fn log(category: &str, message: &str) {
    let line = format!("{} [{}] {}\n", now_stamp(), category, message);
    let dir = log_dir();
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
    s.replace("password=", "password=[REDACTED]")
        .replace("passphrase=", "passphrase=[REDACTED]")
}

// --- Tauri command ---

#[derive(Debug, Serialize)]
pub struct OpsLogInfo {
    pub dir: String,
    pub latest: String,
    pub session: Option<String>,
    /// `env` | `dev_tree` | `app_data` | `fallback`
    pub source: LogDirSource,
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
    let dir = log_dir();
    let latest = dir.join("latest.log");
    let session = STATE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.session_path.display().to_string()));
    Ok(OpsLogInfo {
        dir: dir.display().to_string(),
        latest: latest.display().to_string(),
        session,
        source: log_dir_source(),
    })
}

/// Open the resolved ops-log directory in the system file manager.
///
/// Uses a native shell command so it does not depend on `plugin-opener` path
/// scopes (opener:default does not allow `open_path`).
#[tauri::command]
pub fn open_ops_log_dir() -> Result<String, String> {
    let dir = log_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    let path_str = dir.display().to_string();

    #[cfg(windows)]
    {
        // `explorer <dir>` opens the folder. spawn (do not wait): explorer often
        // returns non-zero even on success.
        std::process::Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("无法启动资源管理器: {e}"))?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("无法打开 Finder: {e}"))?;
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("无法打开文件管理器: {e}"))?;
    }

    log("UI", &format!("open_ops_log_dir path={path_str}"));
    Ok(path_str)
}

/// Convenience macros-like helpers used from Rust modules.
#[macro_export]
macro_rules! ops {
    ($cat:expr, $($arg:tt)*) => {{
        $crate::ops_log::log($cat, &format!($($arg)*));
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn env_override_wins() {
        let (dir, src) = resolve_log_dir_with(Some(r"D:\custom\logs"), None);
        assert_eq!(src, LogDirSource::Env);
        assert_eq!(dir, PathBuf::from(r"D:\custom\logs"));
    }

    #[test]
    fn empty_env_falls_through() {
        let (dir, src) = resolve_log_dir_with(Some("  "), None);
        assert_ne!(src, LogDirSource::Env);
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    fn dev_tree_from_target_release_layout() {
        let tmp = std::env::temp_dir().join(format!(
            "anchorterm-ops-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let root = tmp.join("repo");
        let release = root
            .join("src-tauri")
            .join("target")
            .join("release");
        fs::create_dir_all(&release).unwrap();
        fs::write(root.join("package.json"), "{}").unwrap();
        fs::create_dir_all(root.join("src-tauri")).unwrap();
        let exe = release.join("anchorterm.exe");
        fs::write(&exe, b"").unwrap();

        let (dir, src) = resolve_log_dir_with(None, Some(&exe));
        assert_eq!(src, LogDirSource::DevTree);
        assert_eq!(dir, root.join("操作日志"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn non_repo_exe_uses_appdata_or_fallback() {
        let tmp = std::env::temp_dir().join(format!(
            "anchorterm-ops-install-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let exe = tmp.join("anchorterm.exe");
        fs::write(&exe, b"").unwrap();

        let (dir, src) = resolve_log_dir_with(None, Some(&exe));
        assert!(
            matches!(src, LogDirSource::AppData | LogDirSource::Fallback),
            "expected AppData/Fallback, got {src:?} dir={}",
            dir.display()
        );
        if src == LogDirSource::AppData {
            assert!(
                dir.ends_with(Path::new("AnchorTerm").join("logs"))
                    || dir.to_string_lossy().contains("AnchorTerm"),
                "dir={}",
                dir.display()
            );
        }

        let _ = fs::remove_dir_all(&tmp);
    }
}
