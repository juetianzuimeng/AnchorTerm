//! Interactive SSH via the system OpenSSH client (`ssh -tt`).
//!
//! Root causes of "connect then reconnect" from ops logs:
//! 1. OpenSSH rejects keys with open Windows ACLs (`UNPROTECTED PRIVATE KEY FILE`)
//! 2. We marked Connected immediately on process spawn, before auth finished
//! 3. Auth failure exit(255) was treated as a network drop → auto-reconnect loop
//!
//! Fixes: secure temp key copy + ACL lock-down; wait for auth; auth fail → Failed (no reconnect).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

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

    let dest = std::env::temp_dir().join(format!("anchorterm-key-{}.pem", Uuid::new_v4()));
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

    Ok(SecureKeyMaterial { path: dest })
}

/// Restrict ACL so OpenSSH on Windows accepts the key file.
fn lockdown_private_key_acl(path: &Path) {
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
        match std::process::Command::new("icacls").args(args).output() {
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

fn is_auth_failure_text(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("permission denied")
        || l.contains("bad permissions")
        || l.contains("unprotected private key")
        || l.contains("too open")
        || l.contains("authentication failed")
        || l.contains("connection refused")
        || l.contains("could not resolve")
        || l.contains("no route to host")
        || l.contains("connection timed out")
        || l.contains("host key verification failed")
}

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
    if l.contains("permission denied") {
        return AppError::Auth(
            "认证失败：公钥被拒绝或私钥口令错误。\
             请确认：1) 私钥与服务器 authorized_keys 匹配；2) 加密私钥已填写正确口令；\
             3) 用户名正确（当前场景 Xshell 可用时，优先检查口令是否填入 AnchorTerm）。"
                .into(),
        );
    }
    if l.contains("connection refused") || l.contains("connection timed out") {
        return AppError::Connect("无法连接主机（拒绝或超时），请检查 IP/端口/防火墙。".into());
    }
    let code = exit
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".into());
    let snippet = stderr.chars().take(400).collect::<String>();
    AppError::Auth(format!(
        "SSH 连接失败 (exit={code}): {snippet}"
    ))
}

struct BuiltArgs {
    args: Vec<String>,
    askpass: Option<AskPassMaterial>,
    secure_key: Option<SecureKeyMaterial>,
}

fn build_ssh_args(params: &ConnectParams) -> Result<BuiltArgs, AppError> {
    // Dead-link detection (e.g. unplugged NIC): plain TCP can stay ESTABLISHED for a
    // long time with no local I/O. OpenSSH client keepalives force a probe so the
    // child exits and we flip UI off "已连接".
    // Worst-case notice ≈ ServerAliveInterval * ServerAliveCountMax (+ RTT).
    // 5s × 2 ≈ ~10s after the path is actually dead.
    let mut args = vec![
        "-tt".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ServerAliveInterval=5".into(),
        "-o".into(),
        "ServerAliveCountMax=2".into(),
        "-o".into(),
        "TCPKeepAlive=yes".into(),
        "-o".into(),
        "NumberOfPasswordPrompts=1".into(),
        "-p".into(),
        params.port.to_string(),
    ];

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
            let secure = prepare_secure_key(
                Path::new(private_key_path),
                passphrase.as_deref(),
            )?;
            args.push("-i".into());
            args.push(secure.path.display().to_string());
            args.push("-o".into());
            args.push("IdentitiesOnly=yes".into());
            args.push("-o".into());
            args.push("PreferredAuthentications=publickey".into());
            args.push("-o".into());
            args.push("PasswordAuthentication=no".into());
            // No askpass: key is already unencrypted.
            (None, Some(secure))
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
pub async fn connect_openssh(
    app: AppHandle,
    params: ConnectParams,
    session_id: String,
) -> Result<OpensshTransport, AppError> {
    let ssh = find_ssh()?;
    let built = build_ssh_args(&params)?;

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
            "openssh connect begin sid={} host={} port={} user={} auth={}",
            &session_id[..session_id.len().min(8)],
            params.host,
            params.port,
            params.username,
            match &params.auth {
                AuthMethod::Password { .. } => "password",
                AuthMethod::PublicKey { .. } => "public_key",
            }
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

    // stdout → UI + buffer
    let app_out = app.clone();
    let sid_out = session_id.clone();
    let out_b = Arc::clone(&out_buf);
    tokio::spawn(async move {
        pump_stream(app_out, sid_out, stdout, Some(out_b)).await;
    });

    // stderr → UI + buffer
    let app_err = app.clone();
    let sid_err = session_id.clone();
    let err_b = Arc::clone(&err_buf);
    tokio::spawn(async move {
        pump_stream(app_err, sid_err, stderr, Some(err_b)).await;
    });

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
        "openssh auth ok; session ready (ServerAliveInterval=5 CountMax=2 TCPKeepAlive=yes)",
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

async fn pump_stream<R: AsyncReadExt + Unpin>(
    app: AppHandle,
    session_id: String,
    mut stream: R,
    mirror: Option<Arc<Mutex<String>>>,
) {
    let mut buf = vec![0u8; 8192];
    let mut total: u64 = 0;
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => {
                crate::ops_log::log(
                    "SSH",
                    &format!(
                        "ssh stream EOF (pump end) sid={} bytes_read={total}",
                        &session_id[..session_id.len().min(8)]
                    ),
                );
                break;
            }
            Ok(n) => {
                total += n as u64;
                if let Some(ref m) = mirror {
                    let mut g = m.lock().await;
                    // Cap diagnostic buffer.
                    if g.len() < 32_000 {
                        g.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
                on_data(&app, &session_id, &buf[..n]);
            }
            Err(e) => {
                warn!(error = %e, "ssh stream read error");
                crate::ops_log::log(
                    "ERR",
                    &format!("ssh stream read error: {e} (bytes_read={total})"),
                );
                break;
            }
        }
    }
}

fn on_data(app: &AppHandle, session_id: &str, data: &[u8]) {
    crate::ops_log::log(
        "ECHO",
        &format!(
            "remote sid={} len={} hex={} text=\"{}\"",
            &session_id[..session_id.len().min(8)],
            data.len(),
            crate::ops_log::hex_preview(data, 96),
            crate::ops_log::text_preview(data, 200)
        ),
    );
    // PR2: object payload with session_id (frontend must route by id).
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
    let Some(state) = app.try_state::<AppState>() else {
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
    if rt.cwd_freeze.load(Ordering::SeqCst) {
        // Still scan for failures? No — freeze means restore playbook owns cwd.
        // OSC is ignored; optimistic rollbacks are irrelevant during freeze.
        return;
    }
    // Release cwd lock before snapshot() — snapshot also locks cwd (non-reentrant).
    let change = {
        let mut cwd = match rt.cwd.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        cwd.feed_output(data)
    };
    if let Some(ch) = change {
        apply_cwd_change(app, &rt, &ch);
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
        crate::ops_log::log(
            "CWD",
            &format!(
                "finish_session freeze restore_target path={path} manual={is_manual} auto={auto}"
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
        crate::ops_log::log("STATE", "finish_session disconnected → will reconnect");
    } else {
        meta.state = SessionState::Disconnected;
        meta.message = Some("连接已断开".into());
        crate::ops_log::log("STATE", "finish_session disconnected (no auto reconnect)");
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
    let _ = app.emit("session://state", snap);
    if do_reconnect {
        crate::session::spawn_reconnect_loop(app, session_id);
    }
}

/// One-shot remote command via OpenSSH (non-interactive).
pub async fn openssh_exec(
    params: &ConnectParams,
    remote_command: &str,
) -> Result<String, AppError> {
    let ssh = find_ssh()?;
    let built = build_ssh_args(params)?;
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
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd
        .output()
        .await
        .map_err(|e| AppError::Ssh(format!("ssh exec 失败: {e}")))?;

    drop(built.askpass);
    drop(built.secure_key);

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
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
