//! Transfer helpers: error classification, local path sandbox, retry policy.

use std::path::{Component, Path, PathBuf};

use super::config::McpConfig;
use super::exec::ToolError;
use crate::error::AppError;
use crate::ops_log;

/// Classify scp/ssh/rsync stderr into a structured [`ToolError`].
pub fn classify_transfer_stderr(op: &str, stderr: &str, exit: Option<i32>) -> ToolError {
    let lower = stderr.to_ascii_lowercase();
    let detail = stderr.trim();

    if lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("access denied")
    {
        return ToolError::permission_denied(format!("{op}: permission denied"))
            .with_detail(detail);
    }
    if lower.contains("no space left")
        || lower.contains("disk quota exceeded")
        || lower.contains("not enough space")
    {
        return ToolError::disk_full(format!("{op}: disk full or quota exceeded")).with_detail(detail);
    }
    if lower.contains("no such file")
        || lower.contains("not a regular file")
        || lower.contains("is a directory")
        || lower.contains("path does not exist")
    {
        return ToolError::file_not_found(format!("{op}: path not found or wrong type"))
            .with_detail(detail);
    }
    if lower.contains("connection reset")
        || lower.contains("connection timed out")
        || lower.contains("connection refused")
        || lower.contains("broken pipe")
        || lower.contains("network is unreachable")
        || lower.contains("connection closed")
        || lower.contains("software caused connection abort")
    {
        return ToolError::transfer_failed(format!("{op}: network error"))
            .with_detail(detail)
            .mark_retryable();
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return ToolError::timeout(format!("{op}: timed out")).with_detail(detail);
    }
    if lower.contains("host key verification failed") {
        return ToolError::permission_denied(format!("{op}: host key verification failed"))
            .with_detail(detail);
    }

    ToolError::transfer_failed(format!(
        "{op} failed (exit={exit:?}): {}",
        if detail.is_empty() { "unknown error" } else { detail }
    ))
    .with_detail(detail)
}

/// Map generic AppError / timeout messages for transfer context.
pub fn map_app_error_transfer(e: AppError) -> ToolError {
    match e {
        AppError::Message(m) if m.contains("timeout") || m.contains("超时") => {
            ToolError::timeout(m).mark_retryable()
        }
        AppError::Message(m)
            if m.contains("取消") || m.to_ascii_lowercase().contains("cancel") =>
        {
            ToolError::cancelled(m)
        }
        AppError::NotConnected => ToolError::not_connected(e.to_string()).mark_retryable(),
        AppError::SessionNotFound(s) => ToolError::not_found(s),
        AppError::Ssh(m) => classify_transfer_stderr("ssh", &m, None),
        other => ToolError::internal(other.to_string()),
    }
}

/// Default sandbox roots when config does not list prefixes.
pub fn default_sandbox_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dl) = dirs::download_dir() {
        roots.push(dl.join("AnchorTerm"));
    }
    if let Some(local) = dirs::data_local_dir() {
        roots.push(local.join("AnchorTerm").join("transfers"));
    }
    if roots.is_empty() {
        roots.push(std::env::temp_dir().join("AnchorTerm").join("transfers"));
    }
    roots
}

fn configured_roots(cfg: &McpConfig) -> Vec<PathBuf> {
    if let Some(ref prefixes) = cfg.transfer_allow_local_prefixes {
        let list: Vec<PathBuf> = prefixes
            .iter()
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    default_sandbox_roots()
}

/// Canonicalize when possible; otherwise absolute normalized path.
pub fn normalize_local_path(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    if path.is_absolute() {
        return normalize_dots(path);
    }
    std::env::current_dir()
        .map(|cwd| normalize_dots(&cwd.join(path)))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn normalize_dots(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn strip_verbatim_prefix(s: &str) -> &str {
    s.strip_prefix(r"\\?\")
        .or_else(|| s.strip_prefix("//?/"))
        .unwrap_or(s)
}

fn is_under_root(path: &Path, root: &Path) -> bool {
    let p = normalize_local_path(path);
    let r = normalize_local_path(root);
    if p.starts_with(&r) {
        return true;
    }
    // Windows: canonicalize may inject `\\?\` on one side only; compare loosely.
    let mut ps = strip_verbatim_prefix(&p.to_string_lossy()).replace('/', "\\");
    let mut rs = strip_verbatim_prefix(&r.to_string_lossy()).replace('/', "\\");
    while ps.ends_with('\\') {
        ps.pop();
    }
    while rs.ends_with('\\') {
        rs.pop();
    }
    if cfg!(windows) {
        ps = ps.to_ascii_lowercase();
        rs = rs.to_ascii_lowercase();
    }
    ps == rs || ps.starts_with(&(rs + std::path::MAIN_SEPARATOR_STR))
}

/// Ensure local path is allowed by sandbox (when enabled).
///
/// - `for_write`: destination (download); may create parent under root.
/// - Relative paths resolve under the first sandbox root.
pub fn check_local_path(
    cfg: &McpConfig,
    path: &Path,
    for_write: bool,
) -> Result<PathBuf, ToolError> {
    let rw = if for_write { "write" } else { "read" };
    if !cfg.transfer_sandbox_enabled {
        let p = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|c| c.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        ops_log::log(
            "MCP",
            &format!(
                "transfer sandbox=off mode={rw} path={}",
                p.display()
            ),
        );
        return Ok(p);
    }

    let roots = configured_roots(cfg);
    // Ensure roots exist for writes.
    if for_write {
        for r in &roots {
            let _ = std::fs::create_dir_all(r);
        }
    }

    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        roots[0].join(path)
    };

    let allowed = roots.iter().any(|r| is_under_root(&candidate, r));
    if !allowed {
        let roots_disp: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        ops_log::log(
            "MCP",
            &format!(
                "transfer sandbox=deny mode={rw} path={} roots={}",
                candidate.display(),
                roots_disp.join("|")
            ),
        );
        return Err(ToolError::sandbox_denied(format!(
            "local path outside sandbox: {} (allowed prefixes: {})",
            candidate.display(),
            roots_disp.join("; ")
        )));
    }
    ops_log::log(
        "MCP",
        &format!(
            "transfer sandbox=allow mode={rw} path={}",
            candidate.display()
        ),
    );
    Ok(candidate)
}

/// Whether this error should be retried under transfer policy.
pub fn is_retryable_transfer_err(e: &ToolError) -> bool {
    e.retryable
        && matches!(
            e.code,
            "timeout" | "busy" | "not_connected" | "transfer_failed"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_permission() {
        let e = classify_transfer_stderr("scp", "Permission denied (publickey)", Some(1));
        assert_eq!(e.code, "permission_denied");
        assert!(!e.retryable);
    }

    #[test]
    fn classify_network_retryable() {
        let e = classify_transfer_stderr("scp", "Connection reset by peer", Some(255));
        assert!(e.retryable);
        assert_eq!(e.code, "transfer_failed");
    }

    #[test]
    fn classify_disk() {
        let e = classify_transfer_stderr("scp", "No space left on device", Some(1));
        assert_eq!(e.code, "disk_full");
    }

    #[test]
    fn sandbox_relative_under_root() {
        let mut cfg = McpConfig::default();
        cfg.transfer_sandbox_enabled = true;
        let tmp = std::env::temp_dir().join("anchorterm-sandbox-test-root");
        let _ = std::fs::create_dir_all(&tmp);
        cfg.transfer_allow_local_prefixes = Some(vec![tmp.display().to_string()]);
        let p = check_local_path(&cfg, Path::new("foo.bin"), true).unwrap();
        assert!(p.starts_with(&tmp));
    }

    #[test]
    fn sandbox_blocks_outside() {
        let mut cfg = McpConfig::default();
        cfg.transfer_sandbox_enabled = true;
        cfg.transfer_allow_local_prefixes =
            Some(vec![std::env::temp_dir().join("only-here").display().to_string()]);
        let err = check_local_path(&cfg, Path::new(r"C:\Windows\System32\drivers"), true);
        assert!(err.is_err());
        assert_eq!(err.unwrap_err().code, "sandbox_denied");
    }
}
