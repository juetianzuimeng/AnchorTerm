//! Interactive SSH via the system OpenSSH client (`ssh -tt`).
//!
//! Root causes of "connect then reconnect" from ops logs:
//! 1. OpenSSH rejects keys with open Windows ACLs (`UNPROTECTED PRIVATE KEY FILE`)
//! 2. We marked Connected immediately on process spawn, before auth finished
//! 3. Auth failure exit(255) was treated as a network drop → auto-reconnect loop
//!
//! Fixes: secure temp key copy + ACL lock-down; wait for auth; auth fail → Failed (no reconnect).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::app_state::{AppState, CwdEvent, DataEvent, SessionSnapshot, SessionState};
use crate::auth::AuthMethod;
use crate::error::AppError;
use crate::ssh::transport::{ConnectParams, SessionCommand};

/// Live OpenSSH session: stdin is the remote PTY keyboard.
pub struct OpensshTransport {
    pub cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    pub stdin: Arc<Mutex<ChildStdin>>,
    pub alive: Arc<AtomicBool>,
    /// Secure temp key copy — deleted when session ends.
    pub secure_key: Option<SecureKeyMaterial>,
}

/// Temp private-key file with locked ACLs; removed on Drop.
pub struct SecureKeyMaterial {
    path: PathBuf,
}

impl SecureKeyMaterial {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SecureKeyMaterial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        crate::ops_log::log(
            "SSH",
            &format!("secure key temp removed path={}", self.path.display()),
        );
    }
}

struct AskPassMaterial {
    askpass_path: PathBuf,
    secret_path: PathBuf,
}

impl Drop for AskPassMaterial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.secret_path);
        let _ = std::fs::remove_file(&self.askpass_path);
    }
}

/// Locate system OpenSSH client. Used by interactive connect and startup check.
pub fn find_ssh() -> Result<PathBuf, AppError> {
    let candidates = [
        r"C:\Windows\System32\OpenSSH\ssh.exe",
        r"C:\Program Files\Git\usr\bin\ssh.exe",
    ];
    for c in candidates {
        if Path::new(c).is_file() {
            return Ok(PathBuf::from(c));
        }
    }
    which_ssh().ok_or_else(|| {
        AppError::Ssh(
            "未找到 ssh.exe。请安装 Windows「OpenSSH 客户端」可选功能后再连接。".into(),
        )
    })
}

fn which_ssh() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join("ssh.exe");
        if p.is_file() {
            return Some(p);
        }
        let p2 = dir.join("ssh");
        if p2.is_file() {
            return Some(p2);
        }
    }
    None
}

/// Startup / UI probe: is system OpenSSH available?
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshCheckResult {
    pub available: bool,
    pub path: Option<String>,
    pub message: String,
    /// Optional help URL for installing OpenSSH on Windows.
    pub help_url: Option<String>,
}

const OPENSSH_HELP_URL: &str =
    "https://learn.microsoft.com/windows-server/administration/openssh/openssh_install_firstuse";

#[tauri::command]
pub fn check_ssh() -> SshCheckResult {
    match find_ssh() {
        Ok(p) => SshCheckResult {
            available: true,
            path: Some(p.display().to_string()),
            message: "已找到系统 OpenSSH 客户端".into(),
            help_url: None,
        },
        Err(_) => SshCheckResult {
            available: false,
            path: None,
            message: "未找到 ssh.exe。AnchorTerm 的交互终端依赖系统 OpenSSH 客户端，请先安装后再连接。"
                .into(),
            help_url: Some(OPENSSH_HELP_URL.into()),
        },
    }
}

fn make_askpass(secret: &str) -> Result<AskPassMaterial, AppError> {
    let id = Uuid::new_v4();
    let dir = std::env::temp_dir();
    let secret_path = dir.join(format!("anchorterm-sec-{id}.txt"));
    let askpass_path = dir.join(format!("anchorterm-ask-{id}.cmd"));

    std::fs::write(&secret_path, secret.as_bytes())
        .map_err(|e| AppError::Ssh(format!("无法创建凭据临时文件: {e}")))?;

    let secret_disp = secret_path.display().to_string();
    let script = format!("@echo off\r\ntype \"{secret_disp}\"\r\n");
    std::fs::write(&askpass_path, script)
        .map_err(|e| AppError::Ssh(format!("无法创建 askpass: {e}")))?;

    Ok(AskPassMaterial {
        askpass_path,
        secret_path,
    })
}

/// Load/decrypt the private key in-process, export an **unencrypted** OpenSSH PEM
/// to a temp file, and lock ACLs so only the current user can read.
///
/// Why not just copy + SSH_ASKPASS?
/// Windows OpenSSH + `CREATE_NO_WINDOW` cannot spawn our `.cmd` askpass
/// (`CreateProcessW failed error:2` / `ssh_askpass: posix_spawnp: No such file`).
/// Encrypted keys then fail with `Permission denied (publickey,password)` even
/// when Xshell succeeds. Decrypting here (including legacy DES-EDE3-CBC via
/// `key_loader`) avoids askpass entirely for public-key auth.
fn prepare_secure_key(
    src: &Path,
    passphrase: Option<&str>,
) -> Result<SecureKeyMaterial, AppError> {
    let dest = export_clear_openssh_key(src, passphrase, "anchorterm-key")?;
    Ok(SecureKeyMaterial { path: dest })
}

/// Export an **unencrypted** OpenSSH private key to a temp PEM (ACL locked).
///
/// Used by interactive SSH and by external tools (e.g. Xftp) so the user is not
/// asked for the key passphrase again. Caller owns deletion of the returned path
/// (or wrap in [`SecureKeyMaterial`]).
pub(crate) fn export_clear_openssh_key(
    src: &Path,
    passphrase: Option<&str>,
    file_prefix: &str,
) -> Result<PathBuf, AppError> {
    if !src.is_file() {
        return Err(AppError::Auth(format!(
            "找不到私钥文件: {}",
            src.display()
        )));
    }

    let key = crate::ssh::key_loader::load_private_key(src, passphrase)?;
    let openssh_pem = key
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .map_err(|e| AppError::Auth(format!("导出私钥为 OpenSSH 格式失败: {e}")))?;

    // Prefer app-local tmp for external tools so cleanup sweeps are consistent.
    let dest = external_key_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(format!("{file_prefix}-{}.pem", Uuid::new_v4()));
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&dest, openssh_pem.as_bytes()).map_err(|e| {
        AppError::Ssh(format!(
            "无法写入解密后的私钥临时文件: {e} (dest: {})",
            dest.display()
        ))
    })?;

    lockdown_private_key_acl(&dest);

    let fp = key.fingerprint(russh::keys::HashAlg::Sha256);
    crate::ops_log::log(
        "SSH",
        &format!(
            "secure key prepared (app-decrypted, clear OpenSSH export) path={} algo={:?} fp={} (from {})",
            dest.display(),
            key.algorithm(),
            fp,
            src.display()
        ),
    );

    Ok(dest)
}

fn external_key_dir() -> Result<PathBuf, AppError> {
    let base = dirs::data_local_dir()
        .ok_or_else(|| AppError::Io("无法解析 %LOCALAPPDATA%".into()))?;
    let dir = base.join("AnchorTerm").join("tmp").join("keys");
    std::fs::create_dir_all(&dir).map_err(|e| AppError::Io(format!("创建密钥临时目录失败: {e}")))?;
    Ok(dir)
}

/// Restrict ACL so OpenSSH on Windows accepts the key file.
pub(crate) fn lockdown_private_key_acl(path: &Path) {
    let dest_s = path.display().to_string();
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "User".into());

    // Drop inheritance first, then grant only current user (replace existing).
    let steps: [Vec<String>; 3] = [
        vec![dest_s.clone(), "/inheritance:r".into()],
        vec![dest_s.clone(), "/grant:r".into(), format!("{user}:R")],
        // Best-effort: strip common over-broad principals OpenSSH rejects.
        vec![
            dest_s.clone(),
            "/remove".into(),
            "Authenticated Users".into(),
            "BUILTIN\\Users".into(),
            "Everyone".into(),
            "BUILTIN\\Administrators".into(),
        ],
    ];

    for args in &steps {
        // Without CREATE_NO_WINDOW, each icacls flashes a black console on Tab
        // complete (prepare_secure_key runs per side-channel ssh).
        let mut cmd = std::process::Command::new("icacls");
        cmd.args(args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                crate::ops_log::log(
                    "ERR",
                    &format!("icacls {:?} soft-fail: {}", args, err.trim()),
                );
            }
            Err(e) => {
                crate::ops_log::log("ERR", &format!("icacls spawn failed: {e}"));
            }
        }
    }
}

/// True when ssh output indicates the connect/auth phase already failed
/// (used to abort the wait loop early). Includes network errors.
fn is_auth_failure_text(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("permission denied")
        || l.contains("bad permissions")
        || l.contains("unprotected private key")
        || l.contains("too open")
        || l.contains("authentication failed")
        || l.contains("connection refused")
        || l.contains("connection closed")
        || l.contains("connection reset")
        || l.contains("broken pipe")
        || l.contains("could not resolve")
        || l.contains("no route to host")
        || l.contains("network is unreachable")
        || l.contains("connection timed out")
        || l.contains("operation timed out")
        || l.contains("host key verification failed")
        || l.contains("kex_exchange_identification")
        || l.contains("banner exchange")
}

/// Classify ssh stderr / exit into Auth (usually permanent) vs Connect (retryable).
///
/// Important: "Connection closed by … port 22" after a network blip is **not**
/// a permanent auth failure — auto-reconnect must keep trying.
fn classify_auth_error(stderr: &str, exit: Option<i32>) -> AppError {
    let l = stderr.to_ascii_lowercase();
    if l.contains("unprotected private key")
        || l.contains("bad permissions")
        || l.contains("too open")
    {
        return AppError::Auth(
            "私钥文件权限过宽，OpenSSH 已拒绝使用。\
             应用会尝试用安全临时副本连接；若仍失败，请在资源管理器中对该私钥：\
             右键→属性→安全→高级→禁用继承并删除其他用户权限，仅保留当前用户读取。"
                .into(),
        );
    }
    if l.contains("ssh_askpass") || l.contains("createprocessw failed") {
        return AppError::Auth(
            "认证失败：无法弹出私钥口令程序（SSH_ASKPASS）。\
             请在连接表单填写「私钥口令 / passphrase」后重试（应用会在本地解密密钥）。"
                .into(),
        );
    }
    if l.contains("permission denied") || l.contains("authentication failed") {
        return AppError::Auth(
            "认证失败：公钥被拒绝或私钥口令错误。\
             请确认：1) 私钥与服务器 authorized_keys 匹配；2) 加密私钥已填写正确口令；\
             3) 用户名正确（当前场景 Xshell 可用时，优先检查口令是否填入 AnchorTerm）。"
                .into(),
        );
    }
    if l.contains("host key verification failed") {
        return AppError::Auth(
            "主机密钥校验失败（known_hosts）。请确认是否首次连接或服务器已更换密钥。"
                .into(),
        );
    }

    // Transient / network — keep auto-reconnect alive.
    if l.contains("connection closed")
        || l.contains("connection reset")
        || l.contains("broken pipe")
        || l.contains("connection refused")
        || l.contains("connection timed out")
        || l.contains("operation timed out")
        || l.contains("timed out")
        || l.contains("no route to host")
        || l.contains("network is unreachable")
        || l.contains("could not resolve")
        || l.contains("name or service not known")
        || l.contains("kex_exchange_identification")
        || l.contains("banner exchange")
        || l.contains("software caused connection abort")
    {
        let snippet = stderr.chars().take(280).collect::<String>();
        return AppError::Connect(format!("网络或握手中断: {snippet}"));
    }

    let code = exit
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".into());
    let snippet = stderr.chars().take(400).collect::<String>();
    // Default to Connect so exit=255 / empty stderr during flaky networks
    // does not permanently kill auto-reconnect.
    AppError::Connect(format!("SSH 连接失败 (exit={code}): {snippet}"))
}

struct BuiltArgs {
    args: Vec<String>,
    askpass: Option<AskPassMaterial>,
    secure_key: Option<SecureKeyMaterial>,
}

/// Multiplex mode for OpenSSH ControlMaster (speeds up Tab complete side-channel).
#[derive(Debug, Clone, Copy)]
enum MuxRole {
    /// Interactive PTY: create / own the shared connection.
    Master,
    /// Non-interactive exec: attach to master when available.
    Slave,
}

/// Whether to enable OpenSSH ControlMaster.
///
/// On Windows, stock OpenSSH frequently fails with:
/// `getsockname failed: Not a socket` when ControlMaster/ControlPath is set.
/// Default **off** on Windows; set env `ANCHORTERM_SSH_MUX=1` to force-enable
/// (for builds that support AF_UNIX mux). Non-Windows defaults to **on**.
pub fn control_master_enabled() -> bool {
    if let Ok(v) = std::env::var("ANCHORTERM_SSH_MUX") {
        let t = v.trim();
        if t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off") {
            return false;
        }
        if t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("on") {
            return true;
        }
    }
    #[cfg(windows)]
    {
        false
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Build ssh argv. When `side_key_cache` is set (side-channel exec), reuse a
/// single decrypted temp key for the whole session so Tab complete does not
/// re-run `icacls` three times per keystroke.
///
/// When `control_path` is set, enable ControlMaster multiplexing so side-channel
/// exec reuses the interactive TCP/auth session (major Tab complete speedup).
fn build_ssh_args_inner(
    params: &ConnectParams,
    side_key_cache: Option<&std::sync::Mutex<Option<SecureKeyMaterial>>>,
    control_path: Option<&Path>,
    mux_role: MuxRole,
) -> Result<BuiltArgs, AppError> {
    // Dead-link detection (e.g. unplugged NIC): plain TCP can stay ESTABLISHED for a
    // long time with no local I/O. OpenSSH client keepalives force a probe so the
    // child exits and we flip UI off "已连接".
    // Target: notice within ~2s → ServerAliveInterval=1 × ServerAliveCountMax=2.
    let mut args = vec![
        "-tt".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ServerAliveInterval=1".into(),
        "-o".into(),
        "ServerAliveCountMax=2".into(),
        "-o".into(),
        "TCPKeepAlive=yes".into(),
        "-o".into(),
        "NumberOfPasswordPrompts=1".into(),
        "-p".into(),
        params.port.to_string(),
    ];

    if let Some(cp) = control_path {
        // Master: interactive owns the mux. Slave/auto: Tab complete attaches.
        // ControlPersist keeps the master briefly after the first client exits so
        // a side-channel can still fire during reconnect races; we always
        // `ssh -O exit` on session teardown.
        let master_opt = match mux_role {
            MuxRole::Master => "yes",
            MuxRole::Slave => "auto",
        };
        args.push("-o".into());
        args.push(format!("ControlMaster={master_opt}"));
        args.push("-o".into());
        args.push(format!("ControlPath={}", cp.display()));
        args.push("-o".into());
        args.push("ControlPersist=60".into());
    }

    let (askpass, secure_key) = match &params.auth {
        AuthMethod::Password { password, .. } => {
            let pw = password
                .as_ref()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| AppError::Auth("未提供密码".into()))?;
            args.push("-o".into());
            args.push("PreferredAuthentications=password".into());
            args.push("-o".into());
            args.push("PubkeyAuthentication=no".into());
            (Some(make_askpass(pw)?), None)
        }
        AuthMethod::PublicKey {
            private_key_path,
            passphrase,
        } => {
            // Decrypt in-app and export clear OpenSSH key — no SSH_ASKPASS needed.
            // (Windows OpenSSH cannot spawn our askpass .cmd under CREATE_NO_WINDOW.)
            let key_path = if let Some(cache) = side_key_cache {
                let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
                if g.is_none() {
                    let t0 = std::time::Instant::now();
                    *g = Some(prepare_secure_key(
                        Path::new(private_key_path),
                        passphrase.as_deref(),
                    )?);
                    crate::ops_log::log(
                        "SSH",
                        &format!(
                            "side-channel key prepared (cached for session) ms={}",
                            t0.elapsed().as_millis()
                        ),
                    );
                } else {
                    crate::ops_log::log("SSH", "side-channel key reused (no icacls)");
                }
                g.as_ref()
                    .map(|k| k.path().display().to_string())
                    .ok_or_else(|| AppError::Ssh("side-channel key missing".into()))?
            } else {
                let secure = prepare_secure_key(
                    Path::new(private_key_path),
                    passphrase.as_deref(),
                )?;
                let p = secure.path().display().to_string();
                args.push("-i".into());
                args.push(p);
                args.push("-o".into());
                args.push("IdentitiesOnly=yes".into());
                args.push("-o".into());
                args.push("PreferredAuthentications=publickey".into());
                args.push("-o".into());
                args.push("PasswordAuthentication=no".into());
                return Ok(BuiltArgs {
                    args: {
                        let mut a = args;
                        a.push(format!("{}@{}", params.username, params.host));
                        a
                    },
                    askpass: None,
                    secure_key: Some(secure),
                });
            };
            args.push("-i".into());
            args.push(key_path);
            args.push("-o".into());
            args.push("IdentitiesOnly=yes".into());
            args.push("-o".into());
            args.push("PreferredAuthentications=publickey".into());
            args.push("-o".into());
            args.push("PasswordAuthentication=no".into());
            // Cache owns the SecureKeyMaterial; do not Drop it after this exec.
            (None, None)
        }
    };

    args.push(format!("{}@{}", params.username, params.host));
    Ok(BuiltArgs {
        args,
        askpass,
        secure_key,
    })
}

/// Connect using system OpenSSH; only returns Ok after auth appears successful.
///
/// `session_id` is bound into stdout/stderr pumps and the child-wait task so
/// `on_data` / `finish_session` never touch a global singleton session.
///
/// `control_path`: when set, this session is ControlMaster so Tab complete can
/// multiplex over the same connection.
pub async fn connect_openssh(
    app: AppHandle,
    params: ConnectParams,
    session_id: String,
    control_path: Option<PathBuf>,
) -> Result<OpensshTransport, AppError> {
    let ssh = find_ssh()?;
    let built = build_ssh_args_inner(
        &params,
        None,
        control_path.as_deref(),
        MuxRole::Master,
    )?;

    info!(
        ssh = %ssh.display(),
        host = %params.host,
        user = %params.username,
        session_id = %session_id,
        "openssh connect"
    );
    crate::ops_log::log(
        "SSH",
        &format!(
            "openssh connect begin sid={} host={} port={} user={} auth={} mux={}",
            &session_id[..session_id.len().min(8)],
            params.host,
            params.port,
            params.username,
            match &params.auth {
                AuthMethod::Password { .. } => "password",
                AuthMethod::PublicKey { .. } => "public_key",
            },
            control_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "off".into())
        ),
    );

    let mut cmd = Command::new(&ssh);
    cmd.args(&built.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Local process has no console (CREATE_NO_WINDOW). Without an explicit TERM,
    // OpenSSH may advertise dumb/empty TERM to the remote, which breaks readline,
    // prompts, and some interactive tools. Force a normal xterm TERM.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    if let Some(ref ap) = built.askpass {
        cmd.env("SSH_ASKPASS", &ap.askpass_path);
        cmd.env("SSH_ASKPASS_REQUIRE", "force");
        cmd.env("DISPLAY", "localhost:0");
        cmd.env_remove("SSH_AUTH_SOCK");
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().map_err(|e| {
        crate::ops_log::log("ERR", &format!("ssh spawn failed: {e}"));
        AppError::Ssh(format!("启动 ssh 失败: {e}"))
    })?;

    crate::ops_log::log(
        "SSH",
        &format!("ssh process spawned pid={:?}", child.id()),
    );

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::Ssh("ssh stdin 不可用".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Ssh("ssh stdout 不可用".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Ssh("ssh stderr 不可用".into()))?;

    let stdin = Arc::new(Mutex::new(stdin));
    let alive = Arc::new(AtomicBool::new(true));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();

    // Shared buffers for auth diagnostics (also streamed to UI).
    let err_buf = Arc::new(Mutex::new(String::new()));
    let out_buf = Arc::new(Mutex::new(String::new()));

    // Keep askpass files alive through auth.
    let askpass = built.askpass;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(45)).await;
        drop(askpass);
    });

    // stdout / stderr → dedicated OS threads (own Tokio current-thread runtimes).
    // Running pumps on the shared app runtime was observed to stall mid-tail:
    // progress stopped ~180KB in, no further pump_waiting_read for that stream,
    // while the other stream kept idling — classic "task no longer polled".
    let app_out = app.clone();
    let sid_out = session_id.clone();
    let out_b = Arc::clone(&out_buf);
    spawn_stream_pump(app_out, sid_out, "stdout", stdout, Some(out_b));

    let app_err = app.clone();
    let sid_err = session_id.clone();
    let err_b = Arc::clone(&err_buf);
    spawn_stream_pump(app_err, sid_err, "stderr", stderr, Some(err_b));

    // ---- Wait for auth success (or fail fast) before reporting Connected ----
    // Previously we returned immediately after spawn → UI showed "已连接" then
    // process exit 255 triggered "准备重连".
    let auth_deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    let mut auth_ok = false;
    loop {
        if tokio::time::Instant::now() >= auth_deadline {
            // Still running after 12s without clear failure → assume OK (slow MOTD).
            if child.try_wait().ok().flatten().is_none() {
                auth_ok = true;
                crate::ops_log::log("SSH", "auth wait timeout elapsed; process still alive → ok");
            }
            break;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let err_text = err_buf.lock().await.clone();
                let out_text = out_buf.lock().await.clone();
                let combined = format!("{err_text}\n{out_text}");
                crate::ops_log::log(
                    "ERR",
                    &format!(
                        "ssh exited during auth status={status:?} err_preview={}",
                        crate::ops_log::text_preview(combined.as_bytes(), 300)
                    ),
                );
                alive.store(false, Ordering::SeqCst);
                return Err(classify_auth_error(
                    &combined,
                    status.code(),
                ));
            }
            Ok(None) => {}
            Err(e) => {
                alive.store(false, Ordering::SeqCst);
                return Err(AppError::Ssh(format!("检查 ssh 进程失败: {e}")));
            }
        }

        let err_text = err_buf.lock().await.clone();
        let out_text = out_buf.lock().await.clone();
        let combined = format!("{err_text}\n{out_text}");

        if is_auth_failure_text(&combined) {
            // Give process a moment to exit; then kill.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let code = match child.try_wait() {
                Ok(Some(s)) => s.code(),
                _ => {
                    let _ = child.kill().await;
                    None
                }
            };
            alive.store(false, Ordering::SeqCst);
            crate::ops_log::log("ERR", "auth failure detected in ssh output");
            return Err(classify_auth_error(&combined, code));
        }

        // Heuristic: login banner / prompt means shell is up.
        let ready = out_text.contains("Last login")
            || out_text.contains("Welcome to")
            || out_text.contains("$ ")
            || out_text.contains("# ")
            || out_text.contains("]$")
            || out_text.contains("]$ ");
        if ready {
            auth_ok = true;
            crate::ops_log::log("SSH", "auth/shell ready heuristic matched");
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if !auth_ok {
        // Process exited without being caught above.
        if let Ok(Some(status)) = child.try_wait() {
            let err_text = err_buf.lock().await.clone();
            alive.store(false, Ordering::SeqCst);
            return Err(classify_auth_error(&err_text, status.code()));
        }
        // Still alive but no heuristic — accept.
        crate::ops_log::log("SSH", "auth wait ended; accepting live process");
    }

    // Background: wait for process exit, or kill when `alive` cleared by Disconnect.
    // (stdin shutdown alone can leave ssh hanging and block app exit on Windows.)
    let app_wait = app.clone();
    let sid_wait = session_id.clone();
    let alive_w = Arc::clone(&alive);
    tokio::spawn(async move {
        let mut child = child;
        let mut forced = false;
        let status = loop {
            if !alive_w.load(Ordering::SeqCst) {
                forced = true;
                crate::ops_log::log(
                    "SSH",
                    &format!(
                        "ssh child kill (alive=false) sid={}",
                        &sid_wait[..sid_wait.len().min(8)]
                    ),
                );
                let _ = child.start_kill();
                break child.wait().await;
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                Err(e) => break Err(e),
            }
        };
        alive_w.store(false, Ordering::SeqCst);
        match status {
            Ok(s) => {
                info!(?s, "ssh child exited");
                crate::ops_log::log(
                    "SSH",
                    &format!(
                        "ssh child exited status={s:?} forced_kill={forced}"
                    ),
                );
            }
            Err(e) => {
                warn!(error = %e, "ssh child wait error");
                crate::ops_log::log("ERR", &format!("ssh child wait error: {e}"));
            }
        }
        // Manual disconnect/close already set UI state; avoid reconnect path.
        finish_session(app_wait, sid_wait, forced).await;
    });

    // Control: disconnect marks alive=false + closes stdin; wait task kills child.
    let alive_c = Arc::clone(&alive);
    let stdin_c = Arc::clone(&stdin);
    tokio::spawn(async move {
        control_loop(cmd_rx, alive_c, stdin_c).await;
    });

    // Terminal size is applied once via debounced `resize` from the UI after
    // connect (see session::resize_inner). Do **not** inject extra `stty` here:
    // multiple stty floods after login raced with user commands and left the
    // shell in a bad state (command echo + 2004l, then no new prompt).
    let _ = (params.cols, params.rows);
    crate::ops_log::log(
        "SSH",
        "openssh auth ok; session ready (ServerAliveInterval=1 CountMax=2 TCPKeepAlive=yes)",
    );

    Ok(OpensshTransport {
        cmd_tx,
        stdin,
        alive,
        secure_key: built.secure_key,
    })
}

async fn control_loop(
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
    alive: Arc<AtomicBool>,
    stdin: Arc<Mutex<ChildStdin>>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCommand::Disconnect => {
                crate::ops_log::log(
                    "SSH",
                    "control disconnect: closing stdin; wait task will kill child",
                );
                // Clear alive first so the wait task can start_kill promptly.
                alive.store(false, Ordering::SeqCst);
                let mut g = stdin.lock().await;
                let _ = g.shutdown().await;
                break;
            }
        }
    }
}

/// Run one SSH stream pump on a **dedicated OS thread** with a private
/// current-thread Tokio runtime so the shared app runtime cannot starve it.
fn spawn_stream_pump<R>(
    app: AppHandle,
    session_id: String,
    label: &'static str,
    stream: R,
    mirror: Option<Arc<Mutex<String>>>,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    let name = format!("ssh-pump-{label}-{}", &session_id[..session_id.len().min(8)]);
    let spawn_result = std::thread::Builder::new().name(name.clone()).spawn(move || {
        crate::ops_log::log(
            "SSH",
            &format!(
                "pump_thread_start label={label} sid={} thread={}",
                &session_id[..session_id.len().min(8)],
                std::thread::current().name().unwrap_or("?")
            ),
        );
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                crate::ops_log::log(
                    "ERR",
                    &format!("pump_thread runtime build failed label={label}: {e}"),
                );
                return;
            }
        };
        rt.block_on(pump_stream(app, session_id, label, stream, mirror));
        crate::ops_log::log("SSH", &format!("pump_thread_exit label={label}"));
    });
    if let Err(e) = spawn_result {
        crate::ops_log::log(
            "ERR",
            &format!("pump_thread spawn failed label={label}: {e}"),
        );
    }
}

async fn pump_stream<R: AsyncReadExt + Unpin>(
    app: AppHandle,
    session_id: String,
    label: &'static str,
    mut stream: R,
    mirror: Option<Arc<Mutex<String>>>,
) {
    let mut buf = vec![0u8; 16_384];
    let mut total: u64 = 0;
    let mut last_heartbeat = Instant::now();
    let mut chunks_since_hb: u64 = 0;
    let mut idle_wait_logs: u64 = 0;
    let sid8 = &session_id[..session_id.len().min(8)];
    loop {
        // Timeout so we can log "still alive, waiting for remote".
        let read_result =
            tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
        match read_result {
            Err(_elapsed) => {
                idle_wait_logs += 1;
                if idle_wait_logs <= 3 || idle_wait_logs % 5 == 0 {
                    crate::ops_log::log(
                        "SSH",
                        &format!(
                            "pump_waiting_read label={label} sid={sid8} total={total} idle_waits={idle_wait_logs} ui_queued={} ui_flushed={}",
                            UI_EMIT_QUEUED.load(Ordering::Relaxed),
                            UI_EMIT_FLUSHED.load(Ordering::Relaxed),
                        ),
                    );
                }
                continue;
            }
            Ok(Ok(0)) => {
                crate::ops_log::log(
                    "SSH",
                    &format!(
                        "ssh stream EOF label={label} sid={sid8} bytes_read={total}"
                    ),
                );
                if label == "stdout" {
                    if let Some(state) = app.try_state::<AppState>() {
                        if let Ok(rt) = state.get_runtime(&session_id) {
                            if rt.sep_pending.load(Ordering::Relaxed) {
                                crate::ops_log::log(
                                    "SEP",
                                    &format!(
                                        "pump_eof_while_pending {}",
                                        rt.sep_stats_line("eof", None)
                                    ),
                                );
                            }
                        }
                    }
                }
                break;
            }
            Ok(Ok(n)) => {
                idle_wait_logs = 0;
                total += n as u64;
                chunks_since_hb += 1;
                // Never block the pump on the auth mirror buffer.
                if let Some(ref m) = mirror {
                    if let Ok(mut g) = m.try_lock() {
                        if g.len() < 32_000 {
                            g.push_str(&String::from_utf8_lossy(&buf[..n]));
                        }
                    }
                }
                let t0 = Instant::now();
                // Catch panics so one bad chunk cannot kill the pump thread.
                let on_data_result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        on_data(&app, &session_id, &buf[..n]);
                    }));
                if let Err(payload) = on_data_result {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    crate::ops_log::log(
                        "ERR",
                        &format!(
                            "on_data_panic label={label} sid={sid8} n={n} total={total} err={msg}"
                        ),
                    );
                }
                let on_data_ms = t0.elapsed().as_millis();
                let need_hb = last_heartbeat.elapsed().as_millis() >= 500
                    || on_data_ms >= 30
                    || chunks_since_hb >= 20;
                if need_hb {
                    crate::ops_log::log(
                        "SSH",
                        &format!(
                            "pump_progress label={label} sid={sid8} n={n} total={total} on_data_ms={on_data_ms} chunks_since_hb={chunks_since_hb} ui_queued={} ui_flushed={}",
                            UI_EMIT_QUEUED.load(Ordering::Relaxed),
                            UI_EMIT_FLUSHED.load(Ordering::Relaxed),
                        ),
                    );
                    if label == "stdout" {
                        if let Some(state) = app.try_state::<AppState>() {
                            if let Ok(rt) = state.get_runtime(&session_id) {
                                if rt.sep_pending.load(Ordering::Relaxed) {
                                    crate::ops_log::log(
                                        "SEP",
                                        &format!(
                                            "pump_chunk sid={sid8} {}",
                                            rt.sep_stats_line("pump", None)
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    last_heartbeat = Instant::now();
                    chunks_since_hb = 0;
                }
            }
            Ok(Err(e)) => {
                warn!(error = %e, label, "ssh stream read error");
                crate::ops_log::log(
                    "ERR",
                    &format!(
                        "ssh stream read error label={label} sid={sid8}: {e} (bytes_read={total})"
                    ),
                );
                break;
            }
        }
    }
}

fn on_data(app: &AppHandle, session_id: &str, data: &[u8]) {
    let Some(state) = app.try_state::<AppState>() else {
        // No state yet — emit raw (should be rare).
        emit_data_raw(app, session_id, data);
        return;
    };
    let Ok(rt) = state.get_runtime(session_id) else {
        crate::ops_log::log(
            "ERR",
            &format!(
                "on_data: session not found sid={}",
                &session_id[..session_id.len().min(8)]
            ),
        );
        return;
    };

    let sep_pending = rt.sep_pending.load(Ordering::Relaxed);
    if sep_pending {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rt.sep_last_on_data_ms.store(now_ms, Ordering::Relaxed);
        rt.sep_chunks.fetch_add(1, Ordering::Relaxed);
        rt.sep_bytes_in
            .fetch_add(data.len() as u64, Ordering::Relaxed);
    }

    // Drop remote echo of our silent `stty` injects so the user never sees them.
    let t_filter = std::time::Instant::now();
    let filtered = rt.filter_outgoing_echo(data);
    let filter_ms = t_filter.elapsed().as_millis();
    if filtered.is_empty() {
        if sep_pending {
            rt.sep_empty_drops.fetch_add(1, Ordering::Relaxed);
            crate::ops_log::log(
                "SEP",
                &format!(
                    "on_data_empty_after_filter sid={} in={} filter_ms={} {}",
                    &session_id[..session_id.len().min(8)],
                    data.len(),
                    filter_ms,
                    rt.sep_stats_line("empty_filter", None)
                ),
            );
        }
        return;
    }

    // Rewrite post-command OSC marker into a green separator line (local only).
    let had_marker_bytes = filtered.windows(SEP_OSC_BEL.len()).any(|w| w == SEP_OSC_BEL)
        || filtered.windows(SEP_OSC_ST.len()).any(|w| w == SEP_OSC_ST);
    let (cols, _) = rt.term_size();
    let filtered = rewrite_cmd_separator_marker(&filtered, cols);
    if had_marker_bytes {
        rt.sep_note_marker_injected();
    }
    if filtered.is_empty() {
        if sep_pending {
            rt.sep_empty_drops.fetch_add(1, Ordering::Relaxed);
            crate::ops_log::log(
                "SEP",
                &format!(
                    "on_data_empty_after_rewrite sid={} in={} {}",
                    &session_id[..session_id.len().min(8)],
                    data.len(),
                    rt.sep_stats_line("empty_rewrite", None)
                ),
            );
        }
        return;
    }

    // OpenSSH client keepalive noise (often appears on drop / failed reconnect).
    // Status bar already covers disconnect/reconnect; keep scrollback clean.
    let filtered = strip_openssh_client_noise(&filtered);
    if filtered.is_empty() {
        if sep_pending {
            rt.sep_empty_drops.fetch_add(1, Ordering::Relaxed);
            crate::ops_log::log(
                "SEP",
                &format!(
                    "on_data_empty_after_noise sid={} in={} {}",
                    &session_id[..session_id.len().min(8)],
                    data.len(),
                    rt.sep_stats_line("empty_noise", None)
                ),
            );
        }
        return;
    }

    if sep_pending || rt.sep_pending.load(Ordering::Relaxed) {
        rt.sep_bytes_out
            .fetch_add(filtered.len() as u64, Ordering::Relaxed);
    }

    // Reconnect / cwd-restore: mute MOTD, Last login, timeout spam, silent cd echo.
    // Still track cwd when not frozen (OSC 7 may arrive during muted login).
    let muted = rt.is_ui_muted();
    // Huge remote dumps (grep/tail of multi-MB logs) used to log every 8KB chunk
    // thrice to disk + UI IPC — freezing the app during reconnect. Rate-limit.
    // Never log bulk payload bodies on the pump path (disk+mutex contention
    // under tail floods). Small interactive chunks only.
    let log_echo = filtered.len() <= 256;
    if !muted {
        if log_echo {
            crate::ops_log::log(
                "ECHO",
                &format!(
                    "remote sid={} len={} hex={} text=\"{}\"",
                    &session_id[..session_id.len().min(8)],
                    filtered.len(),
                    crate::ops_log::hex_preview(&filtered, 48),
                    crate::ops_log::text_preview(&filtered, 120)
                ),
            );
        }
        // Non-blocking queue — pump must not wait on WebView.
        emit_data_raw(app, session_id, &filtered);
        if sep_pending && rt.sep_chunks.load(Ordering::Relaxed) <= 3 {
            crate::ops_log::log(
                "SEP",
                &format!(
                    "emit_queued sid={} out={} filter_ms={} {}",
                    &session_id[..session_id.len().min(8)],
                    filtered.len(),
                    filter_ms,
                    rt.sep_stats_line("emit", None)
                ),
            );
        }
    } else if log_echo {
        crate::ops_log::log(
            "ECHO",
            &format!(
                "muted sid={} len={} text=\"{}\"",
                &session_id[..session_id.len().min(8)],
                filtered.len(),
                crate::ops_log::text_preview(&filtered, 120)
            ),
        );
        if sep_pending {
            crate::ops_log::log(
                "SEP",
                &format!(
                    "emit_skipped_muted sid={} out={} {}",
                    &session_id[..session_id.len().min(8)],
                    filtered.len(),
                    rt.sep_stats_line("muted", None)
                ),
            );
        }
    }

    if rt.cwd_freeze.load(Ordering::SeqCst) {
        // Freeze means restore playbook owns cwd; ignore OSC / cd parse from output.
        return;
    }
    // Release cwd lock before snapshot() — snapshot also locks cwd (non-reentrant).
    let change = {
        let mut cwd = match rt.cwd.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        cwd.feed_output(&filtered)
    };
    if let Some(ch) = change {
        apply_cwd_change(app, &rt, &ch);
    }
}

/// Private OSC emitted after user commands when post-separator is enabled.
/// BEL-terminated (common) and ST-terminated (ESC \) forms.
const SEP_OSC_BEL: &[u8] = b"\x1b]733;ATsep\x07";
const SEP_OSC_ST: &[u8] = b"\x1b]733;ATsep\x1b\\";

/// Build a green separator spanning most of the terminal width.
/// Uses U+2500 BOX DRAWINGS LIGHT HORIZONTAL; width is clamped for sanity.
fn sep_line_for_cols(cols: u32) -> String {
    // Leave a small margin so the line does not wrap on the last cell.
    let width = (cols as usize).clamp(60, 240).saturating_sub(2).max(48);
    let mut s = String::with_capacity(width * 3 + 16);
    s.push_str("\r\n\x1b[32m");
    for _ in 0..width {
        s.push('─');
    }
    s.push_str("\x1b[0m\r\n");
    s
}

/// Replace post-command OSC marker(s) with a green separator line.
fn rewrite_cmd_separator_marker(data: &[u8], cols: u32) -> Vec<u8> {
    if find_bytes(data, SEP_OSC_BEL).is_none() && find_bytes(data, SEP_OSC_ST).is_none() {
        return data.to_vec();
    }
    let sep = sep_line_for_cols(cols);
    let mut out = replace_bytes(data, SEP_OSC_BEL, sep.as_bytes());
    out = replace_bytes(&out, SEP_OSC_ST, sep.as_bytes());
    crate::ops_log::log(
        "SEP",
        &format!(
            "rewrite_marker cols={cols} sep_cells={} in={} out={} delta={}",
            (cols as usize).clamp(60, 240).saturating_sub(2).max(48),
            data.len(),
            out.len(),
            out.len() as i64 - data.len() as i64
        ),
    );
    out
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn replace_bytes(hay: &[u8], needle: &[u8], rep: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len().saturating_add(64));
    let mut i = 0;
    while i < hay.len() {
        if i + needle.len() <= hay.len() && &hay[i..i + needle.len()] == needle {
            out.extend_from_slice(rep);
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// Drop OpenSSH client diagnostic lines that are not remote shell output.
/// e.g. `Timeout, server 1.2.3.4 not responding.`
fn strip_openssh_client_noise(data: &[u8]) -> Vec<u8> {
    // Fast path: avoid UTF-8 allocation when the needle is absent (bulk path).
    if !data.windows(15).any(|w| w == b"Timeout, server") {
        return data.to_vec();
    }
    let text = String::from_utf8_lossy(data);
    let mut out = String::with_capacity(text.len());
    // Normalize to \n for line decisions, then rewrite with original endings preserved
    // by scanning the original string with a simple line walker.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find end of line (exclusive of line ending bytes).
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'\n' && bytes[j] != b'\r' {
            j += 1;
        }
        let line = &text[i..j];
        // Consume one line ending: \r\n, \n, or lone \r.
        let mut k = j;
        if k < bytes.len() && bytes[k] == b'\r' {
            k += 1;
            if k < bytes.len() && bytes[k] == b'\n' {
                k += 1;
            }
        } else if k < bytes.len() && bytes[k] == b'\n' {
            k += 1;
        }
        let ending = &text[j..k];
        let trimmed = line.trim();
        let is_timeout =
            trimmed.starts_with("Timeout, server") && trimmed.contains("not responding");
        if !is_timeout {
            out.push_str(line);
            out.push_str(ending);
        }
        i = k;
    }
    out.into_bytes()
}

#[cfg(test)]
mod noise_filter_tests {
    use super::strip_openssh_client_noise;

    #[test]
    fn strips_timeout_lines() {
        let raw = b"Timeout, server 43.106.28.40 not responding.\r\nTimeout, server 43.106.28.40 not responding.\r\n";
        let out = strip_openssh_client_noise(raw);
        assert!(out.is_empty(), "got {:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn keeps_shell_output() {
        let raw = b"ls\r\nfile.txt\r\n";
        let out = strip_openssh_client_noise(raw);
        assert_eq!(out, raw);
    }

    #[test]
    fn strips_timeout_keeps_neighbors() {
        let raw = b"hello\r\nTimeout, server 1.2.3.4 not responding.\r\nworld\r\n";
        let out = strip_openssh_client_noise(raw);
        assert_eq!(String::from_utf8_lossy(&out), "hello\r\nworld\r\n");
    }
}

/// UI terminal bytes: pump → std mpsc → **dedicated OS thread** → WebView.
///
/// Important: `app.emit` must NOT run on a Tokio worker. A slow WebView can block
/// emit for a long time; if that worker also services the SSH stdout pump, the
/// PTY stops being read → remote `tail` blocks → session appears frozen
/// (stdin write still succeeds, but no further echo).
type UiEmitItem = (String, Vec<u8>);
static UI_EMIT_TX: OnceLock<std::sync::mpsc::Sender<UiEmitItem>> = OnceLock::new();
static UI_EMIT_QUEUED: AtomicU64 = AtomicU64::new(0);
static UI_EMIT_FLUSHED: AtomicU64 = AtomicU64::new(0);

fn ensure_ui_emit_worker(app: &AppHandle) -> std::sync::mpsc::Sender<UiEmitItem> {
    UI_EMIT_TX
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<UiEmitItem>();
            let app = app.clone();
            std::thread::Builder::new()
                .name("anchor-ui-emit".into())
                .spawn(move || {
                    ui_emit_worker_loop(app, rx);
                })
                .expect("spawn anchor-ui-emit thread");
            crate::ops_log::log(
                "SSH",
                "ui_emit_worker started (dedicated OS thread, coalesce ~12ms)",
            );
            tx
        })
        .clone()
}

fn ui_emit_worker_loop(app: AppHandle, rx: std::sync::mpsc::Receiver<UiEmitItem>) {
    let mut pending: HashMap<String, Vec<u8>> = HashMap::new();
    let mut last_flood_log = Instant::now();
    let mut flood_bytes: u64 = 0;
    let mut flood_flushes: u64 = 0;
    let mut emit_block_warns: u64 = 0;

    loop {
        // Block for the first item when idle; timed wait when we have a batch
        // so interactive output still flushes promptly.
        let first = if pending.is_empty() {
            match rx.recv() {
                Ok(x) => Some(x),
                Err(_) => break,
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(12)) {
                Ok(x) => Some(x),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };

        if let Some((sid, data)) = first {
            let n = data.len() as u64;
            UI_EMIT_QUEUED.fetch_add(n, Ordering::Relaxed);
            flood_bytes += n;
            pending.entry(sid).or_default().extend(data);
        }

        // Drain whatever is already queued (coalesce a burst into one emit).
        loop {
            match rx.try_recv() {
                Ok((sid, data)) => {
                    let n = data.len() as u64;
                    UI_EMIT_QUEUED.fetch_add(n, Ordering::Relaxed);
                    flood_bytes += n;
                    pending.entry(sid).or_default().extend(data);
                    // Cap RAM if WebView is extremely slow.
                    let total: usize = pending.values().map(|v| v.len()).sum();
                    if total >= 2 * 1024 * 1024 {
                        break;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }

        if pending.is_empty() {
            if last_flood_log.elapsed() >= Duration::from_secs(2) && flood_bytes > 0 {
                crate::ops_log::log(
                    "SSH",
                    &format!(
                        "ui_emit_idle_stats bytes_window={flood_bytes} flushes={flood_flushes} block_warns={emit_block_warns} queued_total={} flushed_total={}",
                        UI_EMIT_QUEUED.load(Ordering::Relaxed),
                        UI_EMIT_FLUSHED.load(Ordering::Relaxed),
                    ),
                );
                flood_bytes = 0;
                flood_flushes = 0;
                last_flood_log = Instant::now();
            }
            continue;
        }

        for (sid, buf) in pending.drain() {
            if buf.is_empty() {
                continue;
            }
            flood_flushes += 1;
            let t0 = Instant::now();
            emit_data_raw_now(&app, &sid, &buf);
            let ms = t0.elapsed().as_millis();
            UI_EMIT_FLUSHED.fetch_add(buf.len() as u64, Ordering::Relaxed);
            if ms >= 100 {
                emit_block_warns += 1;
                crate::ops_log::log(
                    "SSH",
                    &format!(
                        "ui_emit_slow sid={} bytes={} emit_ms={} (OS thread; pump unaffected)",
                        &sid[..sid.len().min(8)],
                        buf.len(),
                        ms
                    ),
                );
            }
        }

        if last_flood_log.elapsed() >= Duration::from_secs(1) && flood_bytes > 64 * 1024 {
            crate::ops_log::log(
                "SSH",
                &format!(
                    "ui_emit_flood_stats bytes_window={flood_bytes} flushes={flood_flushes} block_warns={emit_block_warns} queued_total={} flushed_total={}",
                    UI_EMIT_QUEUED.load(Ordering::Relaxed),
                    UI_EMIT_FLUSHED.load(Ordering::Relaxed),
                ),
            );
            flood_bytes = 0;
            flood_flushes = 0;
            last_flood_log = Instant::now();
        }
    }

    crate::ops_log::log("SSH", "ui_emit_worker exit");
}

/// Queue terminal bytes for the UI. Non-blocking for the SSH stdout pump.
fn emit_data_raw(app: &AppHandle, session_id: &str, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let tx = ensure_ui_emit_worker(app);
    if let Err(e) = tx.send((session_id.to_string(), data.to_vec())) {
        error!("ui emit queue closed: {e}");
        crate::ops_log::log("ERR", &format!("ui emit queue closed: {e}"));
    }
}

fn emit_data_raw_now(app: &AppHandle, session_id: &str, data: &[u8]) {
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
    if let Err(e) = app.emit(
        "session://data",
        DataEvent {
            session_id: session_id.to_string(),
            data_b64: b64,
        },
    ) {
        error!("emit data failed: {e}");
        crate::ops_log::log("ERR", &format!("emit session://data failed: {e}"));
    }
}

/// Apply a cwd tracker change to restore_target + UI events.
fn apply_cwd_change(
    app: &AppHandle,
    rt: &crate::app_state::SessionRuntime,
    ch: &crate::cwd::CwdChange,
) {
    use crate::cwd::CwdChangeReason;
    match ch.reason {
        CwdChangeReason::Osc7 => {
            if let Some(ref path) = ch.path {
                crate::ops_log::log("CWD", &format!("from_osc path={path}"));
            }
        }
        CwdChangeReason::OscTitle => {
            if let Some(ref path) = ch.path {
                crate::ops_log::log("CWD", &format!("from_osc_title path={path}"));
            }
        }
        CwdChangeReason::CdRollback => {
            crate::ops_log::log(
                "CWD",
                &format!(
                    "cd_rollback path={}",
                    ch.path.as_deref().unwrap_or("(none)")
                ),
            );
        }
        CwdChangeReason::CdParse => {
            // Only emitted from submit_line / write path, not on_data.
        }
    }

    match &ch.path {
        Some(path) if path.starts_with('/') => {
            if let Ok(mut target) = rt.restore_target.lock() {
                *target = Some(path.clone());
            }
            // Persist for tab-close → later connect to same host+user.
            {
                let meta = rt.meta.lock().ok();
                if let Some(meta) = meta {
                    if let (Some(host), Some(user)) =
                        (meta.host.as_deref(), meta.username.as_deref())
                    {
                        if !host.is_empty() && !user.is_empty() {
                            crate::config::save_last_cwd(host, user, path);
                        }
                    }
                }
            }
            let _ = app.emit(
                "session://cwd",
                CwdEvent {
                    session_id: rt.id.clone(),
                    cwd: path.clone(),
                },
            );
        }
        Some(path) => {
            // Non-absolute (e.g. ~/x) — emit for UI but do not freeze as restore target.
            let _ = app.emit(
                "session://cwd",
                CwdEvent {
                    session_id: rt.id.clone(),
                    cwd: path.clone(),
                },
            );
        }
        None => {
            // Rolled back to unknown: clear restore freeze path.
            if let Ok(mut target) = rt.restore_target.lock() {
                *target = None;
            }
            let _ = app.emit(
                "session://cwd",
                CwdEvent {
                    session_id: rt.id.clone(),
                    cwd: String::new(),
                },
            );
        }
    }
    let _ = app.emit("session://state", rt.snapshot());
}

async fn finish_session(app: AppHandle, session_id: String, manual: bool) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Ok(rt) = state.get_runtime(&session_id) else {
        crate::ops_log::log(
            "ERR",
            &format!(
                "finish_session: session not found sid={}",
                &session_id[..session_id.len().min(8)]
            ),
        );
        return;
    };
    let had = {
        let mut t = rt.transport.lock().expect("transport lock");
        t.take().is_some()
    };
    if !had {
        return;
    }

    let auto = rt.auto_reconnect.load(Ordering::SeqCst);
    let mut meta = rt.meta.lock().expect("meta lock");
    let is_manual = manual || matches!(meta.state, SessionState::Idle);

    // Always remember absolute cwd on unexpected drop (for auto-reconnect) and
    // also when finishing after manual disconnect path if still available.
    // Release cwd before building snapshot below.
    let frozen_path = {
        let cwd = rt.cwd.lock().ok();
        cwd.and_then(|c| c.last_known().map(|s| s.to_string()))
            .filter(|p| p.starts_with('/'))
    };
    if let Some(ref path) = frozen_path {
        if let Ok(mut target) = rt.restore_target.lock() {
            *target = Some(path.clone());
        }
        {
            let host = meta.host.clone().unwrap_or_default();
            let user = meta.username.clone().unwrap_or_default();
            if !host.is_empty() && !user.is_empty() {
                crate::config::save_last_cwd(&host, &user, path);
            }
        }
        crate::ops_log::log(
            "CWD",
            &format!(
                "finish_session freeze restore_target path={path} manual={is_manual} auto={auto}"
            ),
        );
    } else {
        crate::ops_log::log(
            "CWD",
            &format!(
                "finish_session freeze skipped (no absolute cwd) manual={is_manual} auto={auto}"
            ),
        );
    }

    if is_manual {
        meta.state = SessionState::Idle;
        meta.message = Some("已手动断开".into());
        meta.attempt = 0;
        rt.cwd_freeze.store(false, Ordering::SeqCst);
        crate::ops_log::log("STATE", "finish_session idle (manual)");
    } else if auto {
        meta.state = SessionState::Disconnected;
        meta.message = Some("连接已断开，准备重连…".into());
        // Mute immediately so trailing OpenSSH timeout lines don't hit the UI
        // before the reconnect loop arms mute.
        rt.set_ui_mute(true);
        // Freeze cwd so the new login shell's OSC title (`~`) cannot wipe
        // restore_target before the restore playbook runs.
        rt.cwd_freeze.store(true, Ordering::SeqCst);
        // Invalidate any restore playbook still awaiting side-channel / sleeps.
        let g = rt.bump_restore_gen();
        crate::ops_log::log(
            "STATE",
            &format!("finish_session disconnected → will reconnect restore_gen={g}"),
        );
    } else {
        meta.state = SessionState::Disconnected;
        meta.message = Some("连接已断开".into());
        rt.set_ui_mute(false);
        let g = rt.bump_restore_gen();
        crate::ops_log::log(
            "STATE",
            &format!("finish_session disconnected (no auto reconnect) restore_gen={g}"),
        );
    }

    let snap = SessionSnapshot {
        session_id: rt.id.clone(),
        state: meta.state.clone(),
        host: meta.host.clone(),
        username: meta.username.clone(),
        message: meta.message.clone(),
        cwd: rt
            .cwd
            .lock()
            .ok()
            .and_then(|c| c.last_known().map(|s| s.to_string())),
        attempt: None,
    };
    let do_reconnect = !is_manual && auto;
    drop(meta);
    // Tear down ControlMaster before spawn_reconnect so the next connect can
    // create a fresh mux (stale socket causes side-channel hangs).
    crate::session::shutdown_session_mux_public(&rt);
    let _ = app.emit("session://state", snap);
    if do_reconnect {
        crate::session::spawn_reconnect_loop(app, session_id);
    }
}

/// One-shot remote command via OpenSSH (non-interactive).
#[allow(dead_code)]
pub async fn openssh_exec(
    params: &ConnectParams,
    remote_command: &str,
) -> Result<String, AppError> {
    openssh_exec_inner(params, remote_command, None, None).await
}

/// Side-channel exec that reuses a per-session decrypted key (Tab complete, pwd).
/// Pass `control_path` from the interactive session to multiplex (ControlMaster).
pub async fn openssh_exec_with_key_cache(
    params: &ConnectParams,
    remote_command: &str,
    key_cache: &std::sync::Mutex<Option<SecureKeyMaterial>>,
    control_path: Option<&Path>,
) -> Result<String, AppError> {
    openssh_exec_inner(params, remote_command, Some(key_cache), control_path).await
}

/// Tear down a ControlMaster mux (`ssh -O exit`) and remove the control path file.
pub fn control_master_exit(params: &ConnectParams, control_path: &Path) {
    let Ok(ssh) = find_ssh() else {
        return;
    };
    let mut cmd = std::process::Command::new(&ssh);
    cmd.arg("-O")
        .arg("exit")
        .arg("-o")
        .arg(format!("ControlPath={}", control_path.display()))
        .arg("-p")
        .arg(params.port.to_string())
        .arg(format!("{}@{}", params.username, params.host));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.output() {
        Ok(o) => {
            crate::ops_log::log(
                "SSH",
                &format!(
                    "control_master_exit path={} status={:?} err={}",
                    control_path.display(),
                    o.status,
                    crate::ops_log::text_preview(&o.stderr, 80)
                ),
            );
        }
        Err(e) => {
            crate::ops_log::log("ERR", &format!("control_master_exit spawn failed: {e}"));
        }
    }
    let _ = std::fs::remove_file(control_path);
}

async fn openssh_exec_inner(
    params: &ConnectParams,
    remote_command: &str,
    side_key_cache: Option<&std::sync::Mutex<Option<SecureKeyMaterial>>>,
    control_path: Option<&Path>,
) -> Result<String, AppError> {
    let ssh = find_ssh()?;
    let t0 = std::time::Instant::now();
    let built = build_ssh_args_inner(
        params,
        side_key_cache,
        control_path,
        MuxRole::Slave,
    )?;
    let mut args = built.args;
    // Non-interactive exec: no -tt
    args.retain(|a| a != "-tt");
    args.push(remote_command.to_string());

    let mut cmd = Command::new(&ssh);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    cmd.env("TERM", "xterm-256color");

    if let Some(ref ap) = built.askpass {
        cmd.env("SSH_ASKPASS", &ap.askpass_path);
        cmd.env("SSH_ASKPASS_REQUIRE", "force");
        cmd.env("DISPLAY", "localhost:0");
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Hide console for ssh.exe (and reduce flash when parent is GUI).
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }

    crate::ops_log::log(
        "SSH",
        &format!(
            "side-channel ssh spawn cmd_len={} key_cached={} mux={}",
            remote_command.len(),
            side_key_cache.is_some(),
            control_path
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "off".into())
        ),
    );

    let output = cmd
        .output()
        .await
        .map_err(|e| AppError::Ssh(format!("ssh exec 失败: {e}")))?;

    drop(built.askpass);
    drop(built.secure_key);

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let ms = t0.elapsed().as_millis();
    let mux_hint = if control_path.is_some() && ms < 400 {
        "likely_mux"
    } else if control_path.is_some() {
        "mux_or_full"
    } else {
        "no_mux"
    };
    crate::ops_log::log(
        "SSH",
        &format!(
            "side-channel ssh done ms={ms} status={:?} stdout_len={} hint={mux_hint}",
            output.status,
            stdout.len()
        ),
    );
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        // Tab-complete scripts may exit non-zero while still printing useful
        // stdout (e.g. partial matches). Prefer stdout when present and not an
        // obvious auth failure.
        let lower = err.to_ascii_lowercase();
        let auth_looking = lower.contains("permission denied")
            || lower.contains("authentication")
            || lower.contains("unprotected private key");
        if !auth_looking && !stdout.trim().is_empty() {
            crate::ops_log::log(
                "SSH",
                &format!(
                    "ssh exec non-zero but stdout kept status={:?} err_preview={}",
                    output.status,
                    crate::ops_log::text_preview(err.as_bytes(), 120)
                ),
            );
            return Ok(stdout);
        }
        crate::ops_log::log(
            "ERR",
            &format!("ssh exec failed status={:?} err={}", output.status, err),
        );
        return Err(classify_auth_error(&err, output.status.code()));
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_closed_is_connect_not_auth() {
        let e = classify_auth_error(
            "Connection closed by 43.106.28.40 port 22",
            Some(255),
        );
        assert!(
            matches!(e, AppError::Connect(_)),
            "expected Connect, got {e:?}"
        );
    }

    #[test]
    fn permission_denied_is_auth() {
        let e = classify_auth_error("Permission denied (publickey).", Some(255));
        assert!(matches!(e, AppError::Auth(_)), "expected Auth, got {e:?}");
    }

    #[test]
    fn reset_by_peer_is_connect() {
        let e = classify_auth_error("Connection reset by peer", Some(255));
        assert!(matches!(e, AppError::Connect(_)), "expected Connect, got {e:?}");
    }
}
