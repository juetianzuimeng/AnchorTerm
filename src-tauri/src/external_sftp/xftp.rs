//! Xftp 8 session file builder and process launcher.
//!
//! Spike-validated (Xftp 8.0.0095):
//! - `Xftp.exe "path\to\file.xfp"` opens the session (single-instance handoff)
//! - Session files are UTF-16 LE with BOM
//! - Template base: `default.xfpf` under Documents\NetSarang Computer\8\Xftp\Sessions
//! - `Protocol=1` = SFTP; `[InitialFolder] Remote=` = remote start dir
//! - **AuthMethodList**: password-first `00,11,20,30`; public-key-first `01,11,20,30`
//!   (matches real Xshell sessions that use UserKey)
//! - **UserKey** must be a **name in NetSarang UserKeys** (e.g. `id_rsa_2048`),
//!   **not** an OpenSSH filesystem path — paths make Xftp fall back to Password UI

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::error::AppError;

/// Minimal embedded fallback when the user's `default.xfpf` is missing.
/// Field set matches Xftp 8 `default.xfpf` (Version=8.1).
const EMBEDDED_XFP_TEMPLATE: &str = r#"[SessionInfo]
Version=8.1
Description=Xftp session file
[Host]
CODEPAGE=65001
Type=0
[Transfer]
Type=1
[IgnoreList]
Count=0
[HwCertificates]
Count=0
[InitialFolder]
OpenLocalTab=FALSE
Download=
Local=
SyncBrowsing=FALSE
Remote=
[Bookmark]
Count=0
[Connection]
Pkcs11Pin=
KeyExchange=
Library=0
Port=22
Mac=
Host=
Delegation=FALSE
Pkcs11Middleware=
Protocol=1
AutoRefreshInterval=5
CapiPin=
AutoRefresh=FALSE
RetryTimer=30
Password=
Anonymous=FALSE
UserKeyPassPhrase=
UseMainConnectionOnly=FALSE
Description=
CapiKey=
SftpServerCommand=
RetryCount=5
UseAuthProfile=FALSE
UseAuthenticationAgent=FALSE
Cipher=
TimeOut=60
AnonymousPasswd=abc@foo.bar
AuthMethodList=
Proxy=
UseZip=FALSE
PassiveMode=TRUE
UserKey=
IPV=0
UseCustomSftpServer=FALSE
UserName=
KeepAlive=TRUE
MultipleConnectionNo=2
AuthProfile=
"#;

#[derive(Debug, Clone, Serialize)]
pub struct DetectXftpResult {
    pub found: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    pub template_path: Option<String>,
}

pub fn detect_xftp_install() -> DetectXftpResult {
    let path = find_xftp_exe().map(|p| p.display().to_string());
    let version = path.as_ref().and_then(|p| read_file_version(Path::new(p)));
    let template_path = find_default_xfpf().map(|p| p.display().to_string());
    DetectXftpResult {
        found: path.is_some(),
        path,
        version,
        template_path,
    }
}

pub fn find_xftp_exe() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Program Files (x86) first — Spike hit on this machine.
    if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
        candidates.push(PathBuf::from(&pf86).join(r"NetSarang\Xftp 8\Xftp.exe"));
        candidates.push(PathBuf::from(&pf86).join(r"NetSarang\Xftp 7\Xftp.exe"));
        candidates.push(PathBuf::from(&pf86).join(r"NetSarang Computer\Xftp 8\Xftp.exe"));
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        candidates.push(PathBuf::from(&pf).join(r"NetSarang\Xftp 8\Xftp.exe"));
        candidates.push(PathBuf::from(&pf).join(r"NetSarang\Xftp 7\Xftp.exe"));
        candidates.push(PathBuf::from(&pf).join(r"NetSarang Computer\Xftp 8\Xftp.exe"));
    }

    for c in &candidates {
        if c.is_file() {
            return Some(c.clone());
        }
    }

    // File association: HKLM\Software\Classes\Xftp.xfp\shell\open\command
    if let Some(from_reg) = find_xftp_from_registry() {
        if from_reg.is_file() {
            return Some(from_reg);
        }
    }

    None
}

#[cfg(windows)]
fn find_xftp_from_registry() -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    // Avoid flashing a console: use powershell silently for a one-shot read.
    // CREATE_NO_WINDOW = 0x08000000
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = r#"
$p = (Get-ItemProperty -Path 'HKLM:\Software\Classes\Xftp.xfp\shell\open\command' -ErrorAction SilentlyContinue).'(default)'
if (-not $p) { $p = (Get-ItemProperty -Path 'HKCU:\Software\Classes\Xftp.xfp\shell\open\command' -ErrorAction SilentlyContinue).'(default)' }
if ($p -match '"([^"]+Xftp\.exe)"') { $matches[1] } elseif ($p -match '([A-Za-z]:\\[^"]+Xftp\.exe)') { $matches[1] }
"#;
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

#[cfg(not(windows))]
fn find_xftp_from_registry() -> Option<PathBuf> {
    None
}

fn read_file_version(_path: &Path) -> Option<String> {
    // Optional: keep light; path existence is enough for P0.
    None
}

fn find_default_xfpf() -> Option<PathBuf> {
    let home = dirs::document_dir()?;
    for ver in ["8", "7"] {
        let p = home
            .join("NetSarang Computer")
            .join(ver)
            .join("Xftp")
            .join("Sessions")
            .join("default.xfpf");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn load_template() -> String {
    if let Some(p) = find_default_xfpf() {
        if let Ok(s) = read_utf16_le_file(&p) {
            if s.contains("[Connection]") && s.contains("Protocol=") {
                return s;
            }
        }
        // Some installs may write UTF-8; try lossy UTF-8.
        if let Ok(bytes) = fs::read(&p) {
            if let Ok(s) = String::from_utf8(bytes) {
                if s.contains("[Connection]") {
                    return s;
                }
            }
        }
    }
    EMBEDDED_XFP_TEMPLATE.to_string()
}

fn read_utf16_le_file(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    if bytes.len() < 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty file"));
    }
    let (bom, data) = if bytes[0] == 0xFF && bytes[1] == 0xFE {
        (true, &bytes[2..])
    } else {
        (false, bytes.as_slice())
    };
    if !bom && data.len() >= 2 && data[1] != 0 {
        // Likely not UTF-16 LE
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not utf-16 le",
        ));
    }
    if data.len() % 2 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "odd utf-16 length",
        ));
    }
    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&u16s))
}

fn write_utf16_le_file(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = Vec::with_capacity(2 + text.len() * 2);
    bytes.push(0xFF);
    bytes.push(0xFE);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let mut f = fs::File::create(path)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Resolve a NetSarang **UserKeys** entry name for Xftp `UserKey=`.
///
/// Xftp ignores OpenSSH file paths in `UserKey=` and falls back to Password.
/// Names come from `%USERPROFILE%\Documents\NetSarang Computer\{ver}\SECSH\UserKeys\*.pri`.
pub fn resolve_netsarang_user_key_name(private_key_path: &str) -> Option<String> {
    let path = Path::new(private_key_path);
    let file_name = path.file_name()?.to_string_lossy();
    // Stem without extension (OpenSSH keys may be extensionless or .pem).
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.to_string());
    let base_no_ext = if file_name.contains('.') && path.extension().is_some() {
        stem.clone()
    } else {
        file_name.to_string()
    };

    let dir = find_user_keys_dir()?;
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for ent in rd.flatten() {
            let n = ent.file_name().to_string_lossy().into_owned();
            if let Some(stripped) = n.strip_suffix(".pri") {
                names.push(stripped.to_string());
            } else if let Some(stripped) = n.strip_suffix(".PRI") {
                names.push(stripped.to_string());
            }
        }
    }
    if names.is_empty() {
        return None;
    }

    // 1) Exact match on full filename / stem
    let exact_cands = [base_no_ext.as_str(), stem.as_str(), file_name.as_ref()];
    for cand in exact_cands {
        if let Some(n) = names.iter().find(|n| n.as_str() == cand) {
            return Some(n.clone());
        }
    }

    // 2) Case-insensitive exact
    for cand in [base_no_ext.as_str(), stem.as_str()] {
        if let Some(n) = names.iter().find(|n| n.eq_ignore_ascii_case(cand)) {
            return Some(n.clone());
        }
    }

    // 3) Prefix: path `id_rsa_2048` matches store `id_rsa_2048(加密的)`
    //    or path `id_rsa_2048(加密的)` matches store `id_rsa_2048`
    let mut best: Option<(usize, String)> = None;
    for n in &names {
        let score = if n.starts_with(&base_no_ext) || base_no_ext.starts_with(n.as_str()) {
            // Prefer longer overlap
            n.len().min(base_no_ext.len())
        } else if let Some(prefix) = base_no_ext.split('(').next() {
            let prefix = prefix.trim();
            if !prefix.is_empty() && (n.starts_with(prefix) || n == prefix) {
                prefix.len()
            } else {
                0
            }
        } else {
            0
        };
        if score >= 6 {
            // Avoid tiny false positives
            if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                best = Some((score, n.clone()));
            }
        }
    }
    best.map(|(_, n)| n)
}

fn find_user_keys_dir() -> Option<PathBuf> {
    let home = dirs::document_dir()?;
    for ver in ["8", "7", "6"] {
        let p = home
            .join("NetSarang Computer")
            .join(ver)
            .join("SECSH")
            .join("UserKeys");
        if p.is_dir() {
            return Some(p);
        }
    }
    // Legacy path
    let legacy = home.join("NetSarang").join("Xftp").join("UserKeys");
    if legacy.is_dir() {
        return Some(legacy);
    }
    None
}

/// AuthMethodList values taken from real Xshell 8 sessions on a working install.
pub fn auth_method_list_password() -> &'static str {
    "00,11,20,30"
}

/// Public-key first — same list as sessions that set `UserKey=id_rsa_…`.
pub fn auth_method_list_public_key() -> &'static str {
    "01,11,20,30"
}

/// Apply LaunchContext fields onto a template `.xfp` body.
///
/// `user_key` must be a **NetSarang UserKeys display name** for public-key auth
/// (not an OpenSSH path). `user_key_passphrase` / `login_password` must already
/// be **NetSarang-encrypted** ciphertext (or empty) — plaintext is ignored by Xftp.
pub fn apply_session_fields(
    template: &str,
    host: &str,
    port: u16,
    username: &str,
    remote: Option<&str>,
    user_key: Option<&str>,
    login_password: Option<&str>,
    user_key_passphrase: Option<&str>,
    password_auth: bool,
    description: &str,
) -> String {
    let auth_list = if password_auth {
        auth_method_list_password()
    } else {
        auth_method_list_public_key()
    };

    let mut out = String::with_capacity(template.len() + 128);
    for line in template.lines() {
        // Match keys even when they already have a value (template defaults).
        let replaced = if let Some(rest) = line.strip_prefix("Host=") {
            // Avoid rewriting unrelated keys like "HostKey…"
            if rest.is_empty() || !rest.contains('=') {
                format!("Host={host}")
            } else {
                line.to_string()
            }
        } else if line.starts_with("Port=") && !line.starts_with("FtpPort=") {
            format!("Port={port}")
        } else if line.starts_with("UserName=") {
            format!("UserName={username}")
        } else if line.starts_with("Protocol=") {
            // 1 = SFTP in Xftp session files
            "Protocol=1".to_string()
        } else if line.starts_with("Password=") {
            match login_password {
                Some(p) if !p.is_empty() => format!("Password={p}"),
                _ => "Password=".to_string(),
            }
        } else if line.starts_with("UserKeyPassPhrase=") {
            // Prefer empty (clear temp key). Fallback may embed session passphrase.
            match user_key_passphrase {
                Some(p) if !p.is_empty() => format!("UserKeyPassPhrase={p}"),
                _ => "UserKeyPassPhrase=".to_string(),
            }
        } else if line.starts_with("UserKey=") {
            match user_key {
                Some(k) if !k.is_empty() => format!("UserKey={k}"),
                _ => "UserKey=".to_string(),
            }
        } else if line.starts_with("AuthMethodList=") {
            format!("AuthMethodList={auth_list}")
        } else if line.starts_with("Remote=") {
            match remote {
                Some(r) if !r.is_empty() => format!("Remote={r}"),
                _ => "Remote=".to_string(),
            }
        } else if line.starts_with("Description=") {
            // Prefer connection-level description when empty or default
            if line == "Description=" || line == "Description=Xftp session file" {
                format!("Description={description}")
            } else if line.starts_with("Description=Xftp") {
                format!("Description={description}")
            } else {
                // SessionInfo Description keep as-is unless empty
                line.to_string()
            }
        } else {
            line.to_string()
        };
        out.push_str(&replaced);
        out.push_str("\r\n");
    }
    // Ensure trailing newline style
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out
}

fn tmp_xftp_dir() -> Result<PathBuf, AppError> {
    let base = dirs::data_local_dir().ok_or_else(|| {
        AppError::Io("无法解析 %LOCALAPPDATA%".into())
    })?;
    let dir = base.join("AnchorTerm").join("tmp").join("xftp");
    fs::create_dir_all(&dir).map_err(|e| AppError::Io(format!("创建临时目录失败: {e}")))?;
    Ok(dir)
}

fn sanitize_filename_part(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect()
}

pub fn write_temp_xfp(
    session_id: &str,
    host: &str,
    port: u16,
    username: &str,
    remote: Option<&str>,
    user_key: Option<&str>,
    login_password: Option<&str>,
    user_key_passphrase: Option<&str>,
    password_auth: bool,
) -> Result<PathBuf, AppError> {
    let template = load_template();
    let sid8 = &session_id[..session_id.len().min(8)];
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!(
        "at-{}-{}-{}.xfp",
        sanitize_filename_part(host),
        sanitize_filename_part(sid8),
        ts
    );
    let path = tmp_xftp_dir()?.join(name);
    let desc = format!("AnchorTerm {username}@{host}");
    let body = apply_session_fields(
        &template,
        host,
        port,
        username,
        remote,
        user_key,
        login_password,
        user_key_passphrase,
        password_auth,
        &desc,
    );
    write_utf16_le_file(&path, &body)
        .map_err(|e| AppError::Io(format!("写入 Xftp 会话文件失败: {e}")))?;
    // Restrict ACL when the session file may contain encrypted secrets.
    let has_secret = login_password.map(|p| !p.is_empty()).unwrap_or(false)
        || user_key_passphrase.map(|p| !p.is_empty()).unwrap_or(false);
    if has_secret {
        crate::ssh::openssh::lockdown_private_key_acl(&path);
    }
    Ok(path)
}

pub fn spawn_xftp(exe: &str, xfp: &Path) -> Result<(), AppError> {
    if !Path::new(exe).is_file() {
        return Err(AppError::Message(format!("Xftp 可执行文件不存在: {exe}")));
    }
    if !xfp.is_file() {
        return Err(AppError::Io(format!(
            "会话文件不存在: {}",
            xfp.display()
        )));
    }

    // Pass path as a single argument (association: Xftp.exe "%1").
    let mut cmd = Command::new(exe);
    cmd.arg(xfp);
    if let Some(parent) = Path::new(exe).parent() {
        cmd.current_dir(parent);
    }

    cmd.spawn()
        .map_err(|e| AppError::Message(format!("无法启动 Xftp: {e}")))?;
    Ok(())
}

/// Delete temporary launch artifacts after delay (best-effort).
pub fn schedule_paths_cleanup(paths: Vec<PathBuf>, delay: std::time::Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        for path in &paths {
            let _ = fs::remove_file(path);
            if let Some(dir) = path.parent() {
                sweep_old_temps(dir, std::time::Duration::from_secs(24 * 3600));
            }
        }
    });
}

fn sweep_old_temps(dir: &Path, max_age: std::time::Duration) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for ent in rd.flatten() {
        let p = ent.path();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "xfp" && ext != "pem" {
            continue;
        }
        let Ok(meta) = ent.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() > max_age {
            let _ = fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_fields_password_session() {
        let out = apply_session_fields(
            EMBEDDED_XFP_TEMPLATE,
            "10.0.0.1",
            2222,
            "ops",
            Some("/var/log"),
            None,
            None,
            None,
            true,
            "AnchorTerm ops@10.0.0.1",
        );
        assert!(out.contains("Host=10.0.0.1\r\n") || out.contains("Host=10.0.0.1\n"));
        assert!(out.contains("Port=2222"));
        assert!(out.contains("UserName=ops"));
        assert!(out.contains("Remote=/var/log"));
        assert!(out.contains("Protocol=1"));
        assert!(out.contains("Password=\r\n") || out.contains("Password=\n"));
        assert!(out.contains("AuthMethodList=00,11,20,30"));
        assert!(!out.contains("Password=secret"));
        assert!(out.contains("UserKeyPassPhrase=\r\n") || out.contains("UserKeyPassPhrase=\n"));
    }

    #[test]
    fn apply_fields_password_embedded() {
        let out = apply_session_fields(
            EMBEDDED_XFP_TEMPLATE,
            "h",
            22,
            "u",
            None,
            None,
            Some("s3cret"),
            None,
            true,
            "t",
        );
        assert!(out.contains("Password=s3cret"));
    }

    #[test]
    fn apply_fields_pubkey() {
        let out = apply_session_fields(
            EMBEDDED_XFP_TEMPLATE,
            "h.example",
            22,
            "root",
            None,
            Some("id_rsa_2048"),
            None,
            Some("keypass"),
            false,
            "test",
        );
        assert!(out.contains("UserKey=id_rsa_2048"));
        assert!(!out.contains(r"UserKey=C:\"));
        assert!(out.contains("AuthMethodList=01,11,20,30"));
        assert!(out.contains("UserKeyPassPhrase=keypass"));
    }

    #[test]
    fn auth_lists_match_xshell_samples() {
        assert_eq!(auth_method_list_password(), "00,11,20,30");
        assert_eq!(auth_method_list_public_key(), "01,11,20,30");
    }

    #[test]
    fn resolve_user_key_prefers_exact_basename() {
        // Unit-level: only exercises path parsing when UserKeys dir is absent → None is ok.
        // Integration relies on real NetSarang install; here ensure no panic on empty path.
        let _ = resolve_netsarang_user_key_name("");
        let _ = resolve_netsarang_user_key_name(r"C:\keys\id_rsa_2048");
    }

    #[test]
    fn utf16_roundtrip() {
        let dir = std::env::temp_dir().join("anchorterm-xftp-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("roundtrip.xfp");
        let text = apply_session_fields(
            EMBEDDED_XFP_TEMPLATE,
            "127.0.0.1",
            22,
            "u",
            Some("/tmp"),
            None,
            None,
            None,
            true,
            "t",
        );
        write_utf16_le_file(&path, &text).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[0..2], &[0xFF, 0xFE]);
        let back = read_utf16_le_file(&path).unwrap();
        assert!(back.contains("Host=127.0.0.1"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize_filename_part("a/b:c*"), "a_b_c_");
    }
}
