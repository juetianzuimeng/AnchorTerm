//! Working-directory tracking: OSC 7 (primary) + simple `cd` line parsing (fallback).
//!
//! Optimistic `cd`/`pushd` updates are rolled back when the remote shell reports
//! failure (e.g. `bash: cd: tg: No such file or directory`). Without OSC 7 this
//! is the main defense against a poisoned `restore_target`.

use std::path::{Component, Path};

/// Why `last_known` changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwdChangeReason {
    Osc7,
    CdParse,
    /// Optimistic cd/pushd rolled back after remote error.
    CdRollback,
}

/// A cwd change for UI / restore_target bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CwdChange {
    /// Absolute path after the change, or `None` if unknown after rollback.
    pub path: Option<String>,
    pub reason: CwdChangeReason,
}

/// Tracks remote cwd without logging secrets.
#[derive(Debug, Default, Clone)]
pub struct CwdTracker {
    last_known: Option<String>,
    /// `last_known` before the latest optimistic cd/pushd (for rollback).
    pre_optimistic: Option<String>,
    /// Optimistic path still awaiting confirmation / possible failure echo.
    optimistic_path: Option<String>,
    osc: OscParser,
    /// Incomplete line buffer for scanning shell error messages (UTF-8 lossy).
    line_buf: String,
}

impl CwdTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_known(&self) -> Option<&str> {
        self.last_known.as_deref()
    }

    pub fn set(&mut self, path: impl Into<String>) {
        let p = path.into();
        if !p.is_empty() {
            self.last_known = Some(p);
            self.clear_optimistic();
        }
    }

    pub fn clear(&mut self) {
        self.last_known = None;
        self.clear_optimistic();
        self.osc = OscParser::default();
        self.line_buf.clear();
    }

    fn clear_optimistic(&mut self) {
        self.pre_optimistic = None;
        self.optimistic_path = None;
    }

    /// Feed remote stdout/stderr bytes.
    /// Returns a change when OSC 7 reports a path, or when a failed `cd` is rolled back.
    pub fn feed_output(&mut self, data: &[u8]) -> Option<CwdChange> {
        // OSC 7 is source of truth — clears any pending optimistic path.
        if let Some(path) = self.osc.push(data) {
            self.last_known = Some(path.clone());
            self.clear_optimistic();
            return Some(CwdChange {
                path: Some(path),
                reason: CwdChangeReason::Osc7,
            });
        }

        // Scan printable text for bash/zsh cd failures.
        let text = String::from_utf8_lossy(data);
        self.line_buf.push_str(&text);
        // Cap buffer to avoid unbounded growth on binary noise.
        if self.line_buf.len() > 8192 {
            let keep = self.line_buf.len() - 4096;
            self.line_buf.drain(..keep);
        }

        // Process complete lines (split on \n / \r).
        let mut rolled_back = false;
        loop {
            let Some(pos) = self.line_buf.find(['\n', '\r']) else {
                break;
            };
            let head = self.line_buf[..pos].to_string();
            let mut rest_start = pos;
            let b = self.line_buf.as_bytes();
            while rest_start < b.len() && (b[rest_start] == b'\n' || b[rest_start] == b'\r') {
                rest_start += 1;
            }
            self.line_buf = self.line_buf[rest_start..].to_string();

            if looks_like_cd_failure(&head) && self.rollback_optimistic() {
                rolled_back = true;
                break;
            }
        }
        // Partial line still in buffer may already contain the full error (rare without \n).
        if !rolled_back
            && self.optimistic_path.is_some()
            && looks_like_cd_failure(&self.line_buf)
            && self.rollback_optimistic()
        {
            self.line_buf.clear();
            rolled_back = true;
        }

        if rolled_back {
            return Some(CwdChange {
                path: self.last_known.clone(),
                reason: CwdChangeReason::CdRollback,
            });
        }
        None
    }

    /// Feed a full user-submitted line (without trailing newline).
    /// Applies optimistic cwd for `cd`/`pushd` and remembers prior path for rollback.
    pub fn feed_submitted_line(&mut self, line: &str) -> Option<CwdChange> {
        let path = parse_directory_command(line, self.last_known.as_deref())?;
        self.pre_optimistic = self.last_known.clone();
        self.optimistic_path = Some(path.clone());
        self.last_known = Some(path.clone());
        // Reset line scan so old errors don't false-trigger.
        self.line_buf.clear();
        Some(CwdChange {
            path: Some(path),
            reason: CwdChangeReason::CdParse,
        })
    }

    /// Roll back last optimistic cd if still pending. Returns true if rolled back.
    fn rollback_optimistic(&mut self) -> bool {
        let Some(pending) = self.optimistic_path.take() else {
            self.pre_optimistic = None;
            return false;
        };
        // Only roll back if nothing else (OSC) already replaced last_known.
        if self.last_known.as_ref() == Some(&pending) {
            self.last_known = self.pre_optimistic.take();
            true
        } else {
            self.pre_optimistic = None;
            false
        }
    }
}

/// Detect bash/zsh style directory-change failures.
fn looks_like_cd_failure(line: &str) -> bool {
    // Strip simple CSI sequences for matching (best-effort).
    let plain = strip_ansi_lite(line);
    let l = plain.to_ascii_lowercase();
    // e.g. "-bash: cd: tg: No such file or directory"
    let is_cdish = l.contains("cd:") || l.contains("pushd:") || l.contains("popd:");
    if !is_cdish {
        return false;
    }
    l.contains("no such file")
        || l.contains("not a directory")
        || l.contains("permission denied")
        || l.contains("too many arguments")
}

/// Minimal ANSI CSI stripper for error-line matching.
fn strip_ansi_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for x in chars.by_ref() {
                    if x.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                // OSC ... BEL or ST — skip until BEL
                chars.next();
                for x in chars.by_ref() {
                    if x == '\u{07}' {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Escape a path for POSIX `cd -- '...'`.
pub fn shell_single_quote(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 2);
    out.push('\'');
    for ch in path.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Build restore command: `cd -- 'path'\r` (PTY Enter = CR only).
pub fn restore_cd_command(path: &str) -> String {
    format!("cd -- {}\r", shell_single_quote(path))
}

// --- OSC 7 parser -----------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct OscParser {
    state: OscState,
    body: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum OscState {
    #[default]
    Idle,
    Esc,
    /// Collecting payload after ESC ]
    Osc,
    /// Saw ESC inside OSC payload (maybe ST = ESC \)
    OscEsc,
}

impl OscParser {
    fn push(&mut self, data: &[u8]) -> Option<String> {
        let mut found = None;
        for &b in data {
            match self.state {
                OscState::Idle => {
                    if b == 0x1b {
                        self.state = OscState::Esc;
                    }
                }
                OscState::Esc => {
                    if b == b']' {
                        self.state = OscState::Osc;
                        self.body.clear();
                    } else if b == 0x1b {
                        self.state = OscState::Esc;
                    } else {
                        self.state = OscState::Idle;
                    }
                }
                OscState::Osc => {
                    if b == 0x07 {
                        // BEL
                        if let Some(p) = parse_osc7_payload(&self.body) {
                            found = Some(p);
                        }
                        self.body.clear();
                        self.state = OscState::Idle;
                    } else if b == 0x1b {
                        self.state = OscState::OscEsc;
                    } else {
                        self.body.push(b);
                        if self.body.len() > 4096 {
                            self.body.clear();
                            self.state = OscState::Idle;
                        }
                    }
                }
                OscState::OscEsc => {
                    if b == b'\\' {
                        // ST terminator
                        if let Some(p) = parse_osc7_payload(&self.body) {
                            found = Some(p);
                        }
                        self.body.clear();
                        self.state = OscState::Idle;
                    } else if b == 0x1b {
                        self.body.push(0x1b);
                        self.state = OscState::OscEsc;
                    } else {
                        self.body.push(0x1b);
                        self.body.push(b);
                        self.state = OscState::Osc;
                    }
                }
            }
        }
        found
    }
}

/// Full OSC payload after `ESC ]`, e.g. `7;file://host/path`
fn parse_osc7_payload(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    let rest = s.strip_prefix("7;")?;
    parse_file_url(rest.trim())
}

fn parse_file_url(s: &str) -> Option<String> {
    let rest = s.strip_prefix("file://")?;
    let path = if let Some(idx) = rest.find('/') {
        &rest[idx..]
    } else {
        return None;
    };
    let decoded = percent_decode(path)?;
    if decoded.is_empty() {
        return None;
    }
    Some(decoded)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = from_hex(bytes[i + 1])?;
            let l = from_hex(bytes[i + 2])?;
            out.push((h << 4) | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// --- cd / pushd parsing -----------------------------------------------------

fn parse_directory_command(line: &str, current: Option<&str>) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let (cmd, rest) = split_first_word(line)?;
    match cmd {
        "cd" | "pushd" => parse_cd_args(rest, current),
        _ => None,
    }
}

fn split_first_word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    if let Some(idx) = s.find(char::is_whitespace) {
        Some((&s[..idx], s[idx..].trim_start()))
    } else {
        Some((s, ""))
    }
}

fn parse_cd_args(args: &str, current: Option<&str>) -> Option<String> {
    let args = args.trim();
    // `cd` / `cd ~` → home. Without knowing $HOME we cannot form an absolute path;
    // keep previous absolute base's home prefix when possible (best-effort).
    if args.is_empty() || args == "~" {
        if let Some(cur) = current {
            if let Some(home) = guess_home_from_path(cur) {
                return Some(home);
            }
        }
        return None;
    }
    let mut tokens = tokenize_shell_args(args);
    while tokens
        .first()
        .map(|t| t.starts_with('-') && t != "-")
        .unwrap_or(false)
    {
        tokens.remove(0);
    }
    let target = tokens.first()?.as_str();
    if target == "-" {
        return None;
    }
    resolve_path(target, current)
}

fn tokenize_shell_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '\\' if !in_single => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn resolve_path(target: &str, current: Option<&str>) -> Option<String> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    if target == "~" {
        return None;
    }
    if let Some(rest) = target.strip_prefix("~/") {
        // Logical home-relative path — restore may fail without absolute home.
        return Some(format!("~/{rest}"));
    }

    if target.starts_with('/') {
        return Some(normalize_abs(target));
    }

    let cur = current?;
    if cur.starts_with('~') {
        return None;
    }
    let joined = Path::new(cur).join(target);
    Some(normalize_abs_path(&joined))
}

fn normalize_abs(path: &str) -> String {
    normalize_abs_path(Path::new(path))
}

/// Best-effort `$HOME` from a known absolute path like `/home/user/...` or `/root/...`.
fn guess_home_from_path(path: &str) -> Option<String> {
    let path = path.trim();
    if !path.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.first() == Some(&"home") && parts.len() >= 2 {
        return Some(format!("/home/{}", parts[1]));
    }
    if parts.first() == Some(&"root") {
        return Some("/root".into());
    }
    // Fallback: first path component only is rarely home; leave unknown.
    None
}

fn normalize_abs_path(path: &Path) -> String {
    let mut stack: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::RootDir => stack.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = stack.pop();
            }
            Component::Normal(s) => stack.push(s.to_string_lossy().into_owned()),
            Component::Prefix(_) => {}
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", stack.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc7_bel() {
        let mut t = CwdTracker::new();
        let seq = b"\x1b]7;file://host/tmp/demo\x07";
        let ch = t.feed_output(seq).unwrap();
        assert_eq!(ch.path.as_deref(), Some("/tmp/demo"));
        assert_eq!(ch.reason, CwdChangeReason::Osc7);
        assert_eq!(t.last_known(), Some("/tmp/demo"));
    }

    #[test]
    fn osc7_st() {
        let mut t = CwdTracker::new();
        let seq = b"\x1b]7;file://localhost/var/log\x1b\\";
        assert_eq!(
            t.feed_output(seq).unwrap().path.as_deref(),
            Some("/var/log")
        );
    }

    #[test]
    fn osc7_percent_utf8_chinese() {
        let mut t = CwdTracker::new();
        let path = "/tmp/%E4%B8%AD%E6%96%87";
        let seq = format!("\x1b]7;file://h{path}\x07");
        assert_eq!(t.feed_output(seq.as_bytes()).unwrap().path.unwrap(), "/tmp/中文");
    }

    #[test]
    fn osc7_split_chunks() {
        let mut t = CwdTracker::new();
        assert!(t.feed_output(b"\x1b]7;file://x/a").is_none());
        assert_eq!(
            t.feed_output(b"bc\x07").unwrap().path.as_deref(),
            Some("/abc")
        );
    }

    #[test]
    fn cd_absolute() {
        let mut t = CwdTracker::new();
        assert_eq!(
            t.feed_submitted_line("cd /tmp/foo")
                .unwrap()
                .path
                .as_deref(),
            Some("/tmp/foo")
        );
    }

    #[test]
    fn cd_quoted_spaces() {
        let mut t = CwdTracker::new();
        assert_eq!(
            t.feed_submitted_line("cd '/tmp/my dir'")
                .unwrap()
                .path
                .as_deref(),
            Some("/tmp/my dir")
        );
    }

    #[test]
    fn cd_relative() {
        let mut t = CwdTracker::new();
        t.set("/home/user");
        assert_eq!(
            t.feed_submitted_line("cd projects/app")
                .unwrap()
                .path
                .as_deref(),
            Some("/home/user/projects/app")
        );
    }

    #[test]
    fn cd_dotdot() {
        let mut t = CwdTracker::new();
        t.set("/home/user/a");
        assert_eq!(
            t.feed_submitted_line("cd ..").unwrap().path.as_deref(),
            Some("/home/user")
        );
    }

    #[test]
    fn cd_chinese_path() {
        let mut t = CwdTracker::new();
        assert_eq!(
            t.feed_submitted_line("cd '/data/项目/代码'")
                .unwrap()
                .path
                .as_deref(),
            Some("/data/项目/代码")
        );
        assert_eq!(
            restore_cd_command("/data/项目/代码"),
            "cd -- '/data/项目/代码'\r"
        );
    }

    #[test]
    fn shell_quote() {
        assert_eq!(shell_single_quote("/tmp/a"), "'/tmp/a'");
        assert_eq!(shell_single_quote("/tmp/a'b"), "'/tmp/a'\\''b'");
    }

    #[test]
    fn restore_cmd() {
        assert_eq!(restore_cd_command("/tmp/x"), "cd -- '/tmp/x'\r");
    }

    /// Regression: failed `cd tg` must not poison base for following `cd tg1`.
    #[test]
    fn rollback_failed_cd_then_relative_ok() {
        let mut t = CwdTracker::new();
        t.set("/home/tguser");

        let ch = t.feed_submitted_line("cd tg").unwrap();
        assert_eq!(ch.path.as_deref(), Some("/home/tguser/tg"));
        assert_eq!(ch.reason, CwdChangeReason::CdParse);

        // Same shape as production log.
        let err = b"-bash: cd: tg: No such file or directory\r\n";
        let rb = t.feed_output(err).unwrap();
        assert_eq!(rb.reason, CwdChangeReason::CdRollback);
        assert_eq!(rb.path.as_deref(), Some("/home/tguser"));
        assert_eq!(t.last_known(), Some("/home/tguser"));

        let ok = t.feed_submitted_line("cd tg1").unwrap();
        assert_eq!(ok.path.as_deref(), Some("/home/tguser/tg1"));
    }

    #[test]
    fn rollback_does_not_fire_without_optimistic() {
        let mut t = CwdTracker::new();
        t.set("/home/tguser");
        let err = b"-bash: cd: tg: No such file or directory\r\n";
        assert!(t.feed_output(err).is_none());
        assert_eq!(t.last_known(), Some("/home/tguser"));
    }

    #[test]
    fn osc7_clears_optimistic_without_rollback() {
        let mut t = CwdTracker::new();
        t.set("/home/tguser");
        t.feed_submitted_line("cd tg1").unwrap();
        let osc = b"\x1b]7;file://host/home/tguser/tg1\x07";
        let ch = t.feed_output(osc).unwrap();
        assert_eq!(ch.reason, CwdChangeReason::Osc7);
        // Failure for old name must not roll back OSC-confirmed path.
        assert!(t
            .feed_output(b"-bash: cd: tg: No such file or directory\r\n")
            .is_none());
        assert_eq!(t.last_known(), Some("/home/tguser/tg1"));
    }

    #[test]
    fn looks_like_cd_failure_samples() {
        assert!(looks_like_cd_failure(
            "-bash: cd: tg: No such file or directory"
        ));
        assert!(looks_like_cd_failure(
            "bash: cd: foo: Not a directory"
        ));
        assert!(!looks_like_cd_failure("cd tg1"));
        assert!(!looks_like_cd_failure("ls: cannot access"));
    }
}
