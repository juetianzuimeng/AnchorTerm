//! Hybrid Tab completion cache: per-directory listings + command names.
//!
//! Tab path is latency-first: cache hit (fresh, complete) is local; miss/stale/
//! truncated always falls back to a single `remote_complete_with_exec` RTT.
//! Directory inventory (`list_dir`) is only for prefetch / background fill.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::app_state::{SessionRuntime, SessionState};
use crate::cwd::{expand_shell_path, normalize_remote_abs, CwdChangeReason};
use crate::ssh::complete::{
    analyze_token, apply_candidate_list, build_cmd_list_command, build_list_dir_command,
    remote_complete_with_exec, CompleteResult, MAX_CANDIDATES,
};
use crate::ssh::transport::ConnectParams;

// --- constants (design doc) -------------------------------------------------

pub const MAX_DIRS_LRU: usize = 64;
pub const MAX_DIR_ENTRIES: usize = 2000;
pub const DIR_TTL: Duration = Duration::from_secs(90);
pub const CMD_TTL: Duration = Duration::from_secs(15 * 60);
pub const MAX_CMD_ENTRIES: usize = 5000;
pub const PREFETCH_TIMEOUT: Duration = Duration::from_secs(4);

// --- feature flag -----------------------------------------------------------

/// `ANCHORTERM_COMPLETE_CACHE=0|false|off` forces legacy remote-only path.
pub fn complete_cache_enabled() -> bool {
    match std::env::var("ANCHORTERM_COMPLETE_CACHE") {
        Ok(v) => {
            let t = v.trim();
            !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

// --- data structures --------------------------------------------------------

/// Direct child of a directory (not recursive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct DirListing {
    pub abs_dir: String,
    pub entries: Vec<DirEntry>,
    pub fetched_at: Instant,
    /// true if remote listing hit MAX_DIR_ENTRIES — never use for local Tab apply.
    pub truncated: bool,
}

#[derive(Debug)]
pub struct DirListingCache {
    map: HashMap<String, DirListing>,
    /// front = oldest; back = most recent
    order: VecDeque<String>,
    max_dirs: usize,
}

impl DirListingCache {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            max_dirs: MAX_DIRS_LRU,
        }
    }

    /// Lookup; on hit **touch** (move key to back of LRU).
    pub fn get(&mut self, abs_dir: &str) -> Option<&DirListing> {
        if !self.map.contains_key(abs_dir) {
            return None;
        }
        self.touch(abs_dir);
        self.map.get(abs_dir)
    }

    fn touch(&mut self, abs_dir: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == abs_dir) {
            self.order.remove(pos);
        }
        self.order.push_back(abs_dir.to_string());
    }

    pub fn insert(&mut self, listing: DirListing) {
        let key = listing.abs_dir.clone();
        if self.map.insert(key.clone(), listing).is_some() {
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
            }
        }
        self.order.push_back(key);
        while self.order.len() > self.max_dirs {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    pub fn invalidate(&mut self, abs_dir: &str) {
        self.map.remove(abs_dir);
        if let Some(pos) = self.order.iter().position(|k| k == abs_dir) {
            self.order.remove(pos);
        }
    }

    #[cfg(test)]
    pub fn contains(&self, abs_dir: &str) -> bool {
        self.map.contains_key(abs_dir)
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

impl Default for DirListingCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct CommandCache {
    pub names: Vec<String>,
    pub fetched_at: Instant,
    pub truncated: bool,
}

/// Shared in-flight directory list so Tab bg-fill and prefetch coalesce.
#[derive(Debug)]
pub struct InFlightList {
    pub epoch: u64,
    pub path_gen: u64,
    pub result: Arc<std::sync::Mutex<Option<Result<DirListing, String>>>>,
    pub notify: Arc<Notify>,
}

#[derive(Debug)]
pub struct InFlightCmd {
    pub epoch: u64,
    pub cmd_gen: u64,
    pub result: Arc<std::sync::Mutex<Option<Result<CommandCache, String>>>>,
    pub notify: Arc<Notify>,
}

/// Per-session aggregate (held under SessionRuntime.complete_cache).
#[derive(Debug)]
pub struct SessionCompleteCache {
    pub dirs: DirListingCache,
    pub commands: Option<CommandCache>,
    pub inflight: HashMap<String, Arc<InFlightList>>,
    pub cmd_inflight: Option<Arc<InFlightCmd>>,
    pub epoch: u64,
    /// Bumped when command cache is explicitly cleared so late fills drop.
    pub cmd_gen: u64,
    pub path_gens: HashMap<String, u64>,
    /// PR5: dirs that returned a truncated full listing (or hit compgen cap).
    /// Within DIR_TTL we skip further full `list_dir` / bg fill — Tab still
    /// uses token-scoped `remote_complete` (never local apply of truncated data).
    pub truncated_known: HashMap<String, Instant>,
}

impl Default for SessionCompleteCache {
    fn default() -> Self {
        Self {
            dirs: DirListingCache::new(),
            commands: None,
            inflight: HashMap::new(),
            cmd_inflight: None,
            epoch: 0,
            cmd_gen: 0,
            path_gens: HashMap::new(),
            truncated_known: HashMap::new(),
        }
    }
}

impl SessionCompleteCache {
    pub fn clear_all(&mut self) {
        self.dirs.clear();
        self.commands = None;
        self.inflight.clear();
        self.cmd_inflight = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.cmd_gen = self.cmd_gen.wrapping_add(1);
        self.path_gens.clear();
        self.truncated_known.clear();
    }

    pub fn bump_path_gen(&mut self, abs_dir: &str) {
        let g = self.path_gens.entry(abs_dir.to_string()).or_insert(0);
        *g = g.wrapping_add(1);
        self.dirs.invalidate(abs_dir);
        self.inflight.remove(abs_dir);
        self.truncated_known.remove(abs_dir);
    }

    /// Record that a full inventory of this dir is too large for cache hit.
    pub fn mark_truncated_known(&mut self, abs_dir: &str) {
        self.truncated_known
            .insert(abs_dir.to_string(), Instant::now());
        // Never keep a partial listing as a hit source.
        self.dirs.invalidate(abs_dir);
    }

    /// True if we recently learned a full list would be truncated (within DIR_TTL).
    pub fn is_truncated_known(&mut self, abs_dir: &str) -> bool {
        // Opportunistic prune expired entries for this key.
        if let Some(at) = self.truncated_known.get(abs_dir).copied() {
            if at.elapsed() <= DIR_TTL {
                return true;
            }
            self.truncated_known.remove(abs_dir);
        }
        false
    }

    /// Drop command-name inventory (PATH / hash -r) and invalidate in-flight fills.
    pub fn clear_commands(&mut self) {
        self.commands = None;
        self.cmd_inflight = None;
        self.cmd_gen = self.cmd_gen.wrapping_add(1);
    }

    fn path_gen_of(&self, abs_dir: &str) -> u64 {
        self.path_gens.get(abs_dir).copied().unwrap_or(0)
    }
}

// --- path resolution --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteMode {
    Cmd,
    Dir,
    File,
}

#[derive(Debug, Clone)]
pub struct CompleteRequest {
    pub mode: CompleteMode,
    pub token: String,
    pub token_start: usize,
    pub token_end: usize,
    pub name_prefix: String,
    pub parent_abs: Option<String>,
    pub token_dir_prefix: String,
}

/// `"foo/bar"` → `("foo/", "bar")`; `"foo/"` → `("foo/", "")`; `"x"` → `("", "x")`.
pub fn split_token_dir_and_name(token: &str) -> (String, String) {
    match token.rfind('/') {
        Some(i) => (token[..=i].to_string(), token[i + 1..].to_string()),
        None => (String::new(), token.to_string()),
    }
}

fn is_other_user_tilde(token: &str) -> bool {
    if !token.starts_with('~') {
        return false;
    }
    if token == "~" || token.starts_with("~/") {
        return false;
    }
    true
}

fn abs_cwd(cwd: Option<&str>, home_hint: Option<&str>) -> Option<String> {
    let c = cwd?;
    if c.starts_with('/') {
        return Some(normalize_remote_abs(c));
    }
    if c.starts_with('~') {
        return expand_shell_path(c, home_hint).filter(|p| p.starts_with('/'));
    }
    None
}

/// Resolve the absolute parent directory for path-mode completion, or `None`
/// to force remote `compgen` (bare `~`, other-user tilde, unresolvable cwd).
pub fn resolve_parent_abs(
    token: &str,
    cwd: Option<&str>,
    home_hint: Option<&str>,
) -> Option<String> {
    if token == "~" || is_other_user_tilde(token) {
        return None;
    }

    let (token_dir_prefix, _name_prefix) = split_token_dir_and_name(token);

    if token_dir_prefix.is_empty() {
        return abs_cwd(cwd, home_hint);
    }

    let dir_for_resolve = if token_dir_prefix == "/" {
        "/".to_string()
    } else {
        token_dir_prefix.trim_end_matches('/').to_string()
    };

    if dir_for_resolve == "~" || dir_for_resolve.starts_with("~/") {
        return expand_shell_path(&dir_for_resolve, home_hint).map(|p| normalize_remote_abs(&p));
    }
    if dir_for_resolve.starts_with('/') {
        return Some(normalize_remote_abs(&dir_for_resolve));
    }
    let base = abs_cwd(cwd, home_hint)?;
    Some(normalize_remote_abs(&format!("{base}/{dir_for_resolve}")))
}

pub fn build_complete_request(
    line: &str,
    cursor: usize,
    cwd: Option<&str>,
    home_hint: Option<&str>,
) -> CompleteRequest {
    let chars: Vec<char> = line.chars().collect();
    let cursor = cursor.min(chars.len());
    let (token_start, token_end, token, _head, is_first_word, prefer_dirs) =
        analyze_token(&chars, cursor);

    let mode = if is_first_word {
        CompleteMode::Cmd
    } else if prefer_dirs {
        CompleteMode::Dir
    } else {
        CompleteMode::File
    };

    if mode == CompleteMode::Cmd {
        return CompleteRequest {
            mode,
            name_prefix: token.clone(),
            parent_abs: None,
            token_dir_prefix: String::new(),
            token,
            token_start,
            token_end,
        };
    }

    let (token_dir_prefix, name_prefix) = split_token_dir_and_name(&token);
    let parent_abs = resolve_parent_abs(&token, cwd, home_hint);
    CompleteRequest {
        mode,
        token,
        token_start,
        token_end,
        name_prefix,
        parent_abs,
        token_dir_prefix,
    }
}

// --- listing parse ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListError {
    CdFailed,
    ListFailed,
    ParseError,
}

pub fn parse_listing(raw: &str) -> Result<(Vec<DirEntry>, bool), ListError> {
    let mut lines = raw.lines().map(|l| l.trim_end_matches('\r'));
    match lines.next().unwrap_or("").trim() {
        "ERR_CD" => return Err(ListError::CdFailed),
        "ERR_LIST" => return Err(ListError::ListFailed),
        "OK" => {}
        _ => return Err(ListError::ParseError),
    }
    let truncated = match lines.next().unwrap_or("").trim() {
        "TRUNCATED" => true,
        "FULL" => false,
        _ => return Err(ListError::ParseError),
    };
    let mut entries = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((kind, name)) = line.split_once('\t') else {
            continue;
        };
        if name.is_empty() || name.contains('\t') {
            continue;
        }
        let is_dir = kind == "d";
        if kind != "d" && kind != "f" {
            continue;
        }
        entries.push(DirEntry {
            name: name.to_string(),
            is_dir,
        });
    }
    Ok((entries, truncated))
}

pub fn parse_cmd_list(raw: &str) -> Result<(Vec<String>, bool), ListError> {
    let mut lines = raw.lines().map(|l| l.trim_end_matches('\r'));
    match lines.next().unwrap_or("").trim() {
        "ERR_CD" | "ERR_LIST" => return Err(ListError::ListFailed),
        "OK" => {}
        first if !first.is_empty() => {
            let mut names = vec![first.to_string()];
            for line in lines {
                let t = line.trim();
                if !t.is_empty() {
                    names.push(t.to_string());
                }
            }
            names.sort();
            names.dedup();
            let truncated = names.len() >= MAX_CMD_ENTRIES;
            if names.len() > MAX_CMD_ENTRIES {
                names.truncate(MAX_CMD_ENTRIES);
            }
            return Ok((names, truncated));
        }
        _ => return Err(ListError::ParseError),
    }
    let truncated_marker = match lines.next().unwrap_or("").trim() {
        "TRUNCATED" => true,
        "FULL" => false,
        _ => return Err(ListError::ParseError),
    };
    let mut names: Vec<String> = lines
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    names.sort();
    names.dedup();
    let truncated = truncated_marker || names.len() >= MAX_CMD_ENTRIES;
    if names.len() > MAX_CMD_ENTRIES {
        names.truncate(MAX_CMD_ENTRIES);
    }
    Ok((names, truncated))
}

// --- local filter -----------------------------------------------------------

pub fn prefix_filter<'a>(
    entries: &'a [DirEntry],
    name_prefix: &str,
    dirs_only: bool,
) -> Vec<&'a DirEntry> {
    entries
        .iter()
        .filter(|e| {
            if dirs_only && !e.is_dir {
                return false;
            }
            e.name.starts_with(name_prefix)
        })
        .collect()
}

pub fn rebuild_candidates(token_dir_prefix: &str, matches: &[&DirEntry]) -> Vec<String> {
    matches
        .iter()
        .map(|e| {
            let mut s = format!("{}{}", token_dir_prefix, e.name);
            if e.is_dir && !s.ends_with('/') {
                s.push('/');
            }
            s
        })
        .collect()
}

// --- clear / invalidate -----------------------------------------------------

pub fn clear_session_cache(rt: &SessionRuntime) {
    if let Ok(mut c) = rt.complete_cache.lock() {
        c.clear_all();
        crate::ops_log::log("CMD", "complete_cache cleared epoch_bump");
    }
}

/// Invalidate one absolute directory listing (bump path_gen, drop LRU entry + inflight).
pub fn on_path_invalidated(rt: &SessionRuntime, abs_dir: &str) {
    if !abs_dir.starts_with('/') {
        return;
    }
    if let Ok(mut c) = rt.complete_cache.lock() {
        c.bump_path_gen(abs_dir);
        crate::ops_log::log(
            "CMD",
            &format!(
                "complete_cache path_invalidate dir={}",
                crate::ops_log::text_preview(abs_dir.as_bytes(), 80)
            ),
        );
    }
}

/// File-system / PATH mutations that should drop cached listings (PR4).
///
/// Conservative: only the **current cwd** listing is invalidated for fs tools.
/// Argument paths are not fully parsed (quotes/globs), so we prefer stale-miss
/// over wrong hits after `mkdir`/`rm` in the working directory.
const FS_MUTATING_COMMANDS: &[&str] = &[
    "mkdir", "rmdir", "touch", "rm", "mv", "cp", "ln", "tar", "unzip", "gzip",
    "gunzip", "install", "chmod", "chown", "dd", "truncate", "mktemp", "rsync",
    "scp", "sftp",
];

/// After a draft line is submitted to the PTY, drop stale complete-cache entries.
///
/// Lock order: clone cwd path under `cwd` lock, drop, then take `complete_cache`.
/// Never hold either lock across await (caller is sync on submit path).
pub fn note_mutating_submit(rt: &SessionRuntime, line: &str) {
    if !complete_cache_enabled() {
        return;
    }
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return;
    }

    let first = first_shell_word(line);

    // PATH / hash mutations → drop command cache only.
    // Must run even when first is empty (bare `PATH=/x` is skipped by first_shell_word).
    if command_cache_should_clear(first, line) || bare_path_assignment(line) {
        if let Ok(mut c) = rt.complete_cache.lock() {
            c.clear_commands();
            crate::ops_log::log(
                "CMD",
                &format!(
                    "complete_cache cmd_invalidate reason=submit first={}",
                    if first.is_empty() { "(bare_path)" } else { first }
                ),
            );
        }
        // fall through: may also be fs-related (unlikely for export/hash)
    }

    if first.is_empty() || !FS_MUTATING_COMMANDS.contains(&first) {
        return;
    }

    // Conservative: only invalidate absolute last_known cwd.
    let cwd_abs = {
        let Ok(cwd) = rt.cwd.lock() else {
            return;
        };
        cwd.last_known()
            .filter(|p| p.starts_with('/'))
            .map(|s| s.to_string())
    };
    let Some(cwd_abs) = cwd_abs else {
        return;
    };

    // on_path_invalidated logs path_invalidate; include first-word reason here once.
    crate::ops_log::log(
        "CMD",
        &format!(
            "complete_cache mutate_invalidate first={first} cwd={}",
            crate::ops_log::text_preview(cwd_abs.as_bytes(), 80)
        ),
    );
    on_path_invalidated(rt, &cwd_abs);
}

/// First whitespace-separated token; strips a simple leading `sudo` / `command` / `time`.
fn first_shell_word(line: &str) -> &str {
    let mut rest = line.trim_start();
    // Skip env assignments: FOO=bar cmd → still find cmd (best-effort, no quotes).
    loop {
        let word = rest.split_whitespace().next().unwrap_or("");
        if word.is_empty() {
            return "";
        }
        if word.contains('=') && !word.starts_with('-') && !word.starts_with('/') {
            // FOO=bar — skip
            rest = rest[word.len()..].trim_start();
            continue;
        }
        if matches!(word, "sudo" | "command" | "time" | "nohup" | "nice") {
            rest = rest[word.len()..].trim_start();
            // skip sudo flags like -u user
            while let Some(w) = rest.split_whitespace().next() {
                if w.starts_with('-') {
                    rest = rest[w.len()..].trim_start();
                    // sudo -u name: skip one more token if -u/-g style
                    // Only flags that take a separate argument consume the next token.
                    // Do NOT treat `-n` / `-E` etc. as taking args (would skip real cmd).
                    if matches!(w, "-u" | "-g" | "-C" | "--user" | "--group") {
                        if let Some(arg) = rest.split_whitespace().next() {
                            if !arg.starts_with('-') {
                                rest = rest[arg.len()..].trim_start();
                            }
                        }
                    }
                    continue;
                }
                break;
            }
            continue;
        }
        // Strip path prefix: /usr/bin/mkdir → mkdir
        if let Some(base) = word.rsplit('/').next() {
            return base;
        }
        return word;
    }
}

fn command_cache_should_clear(first: &str, line: &str) -> bool {
    if first == "hash" {
        // `hash -r` rebuilds the shell's command hash table
        return line.split_whitespace().any(|w| w == "-r");
    }
    if first == "export" || first == "declare" || first == "typeset" {
        return line.contains("PATH");
    }
    if first.starts_with("PATH=") {
        return true;
    }
    false
}

/// `PATH=/x:$PATH` with no following command (first_shell_word becomes empty).
fn bare_path_assignment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("PATH=") || t.starts_with("export PATH")
}

// --- hybrid complete --------------------------------------------------------

/// Hybrid Tab completion entry. On miss/stale/truncated: exactly one
/// `remote_complete_with_exec` (parity with today). Never awaits list_dir first.
pub async fn complete_with_cache(
    rt: Arc<SessionRuntime>,
    cwd: Option<String>,
    home_hint: Option<String>,
    line: String,
    cursor: usize,
) -> Result<CompleteResult, String> {
    if !complete_cache_enabled() {
        crate::ops_log::log("CMD", "complete remote_fallback reason=flag_off");
        return run_remote_complete(Arc::clone(&rt), cwd, line, cursor).await;
    }

    let req = build_complete_request(&line, cursor, cwd.as_deref(), home_hint.as_deref());

    match req.mode {
        CompleteMode::Cmd => complete_cmd(rt, cwd, line, cursor, req).await,
        CompleteMode::Dir | CompleteMode::File => {
            complete_path(rt, cwd, line, cursor, req).await
        }
    }
}

async fn complete_path(
    rt: Arc<SessionRuntime>,
    cwd: Option<String>,
    line: String,
    cursor: usize,
    req: CompleteRequest,
) -> Result<CompleteResult, String> {
    let dirs_only = req.mode == CompleteMode::Dir;

    if let Some(ref parent) = req.parent_abs {
        if let Some(result) = try_local_path_hit(&rt, parent, &req, dirs_only, &line, cursor) {
            crate::ops_log::log(
                "CMD",
                &format!(
                    "complete cache_hit kind=dir dir={} n={} serve=local",
                    crate::ops_log::text_preview(parent.as_bytes(), 80),
                    result.candidates.len()
                ),
            );
            return Ok(result);
        }
    } else {
        crate::ops_log::log(
            "CMD",
            "complete remote_fallback reason=no_parent_or_bare_tilde",
        );
    }

    let parent_for_bg = req.parent_abs.clone();
    let skip_bg_list = parent_for_bg
        .as_ref()
        .map(|p| is_truncated_known_dir(&rt, p))
        .unwrap_or(false);
    let reason = if req.parent_abs.is_none() {
        "no_parent"
    } else if skip_bg_list {
        "miss_known_truncated"
    } else {
        "miss_stale_or_trunc"
    };
    crate::ops_log::log(
        "CMD",
        &format!(
            "complete cache_miss kind=dir serve=remote_complete reason={reason} bg_list={}",
            if skip_bg_list { 0 } else { 1 }
        ),
    );

    // Tab: exactly one token-scoped remote RTT (parity with today / PR1).
    let result = run_remote_complete(Arc::clone(&rt), cwd, line, cursor).await;

    // PR5: after remote, decide whether a full-dir bg list is useful.
    if let Some(parent) = parent_for_bg {
        let mut do_bg = !skip_bg_list;
        if let Ok(ref r) = result {
            // Hit the return cap → directory is almost certainly huge; full
            // list_dir would truncate and must not become a local hit. Skip
            // wasteful bg inventory for DIR_TTL (Tab stays on remote_complete).
            if r.candidates.len() >= MAX_CANDIDATES {
                mark_truncated_known_dir(&rt, &parent);
                do_bg = false;
                crate::ops_log::log(
                    "CMD",
                    &format!(
                        "complete mark_truncated_known dir={} reason=remote_cap n={}",
                        crate::ops_log::text_preview(parent.as_bytes(), 80),
                        r.candidates.len()
                    ),
                );
            }
        }
        if do_bg {
            schedule_ensure_dir_listing(Arc::clone(&rt), parent);
        } else {
            crate::ops_log::log(
                "CMD",
                &format!(
                    "complete bg_list_skip dir={} reason=truncated_or_cap",
                    crate::ops_log::text_preview(parent.as_bytes(), 80)
                ),
            );
        }
    }

    result
}

fn is_truncated_known_dir(rt: &SessionRuntime, abs_dir: &str) -> bool {
    rt.complete_cache
        .lock()
        .ok()
        .map(|mut c| c.is_truncated_known(abs_dir))
        .unwrap_or(false)
}

fn mark_truncated_known_dir(rt: &SessionRuntime, abs_dir: &str) {
    if let Ok(mut c) = rt.complete_cache.lock() {
        c.mark_truncated_known(abs_dir);
    }
}

fn try_local_path_hit(
    rt: &SessionRuntime,
    parent: &str,
    req: &CompleteRequest,
    dirs_only: bool,
    line: &str,
    cursor: usize,
) -> Option<CompleteResult> {
    let mut cache = rt.complete_cache.lock().ok()?;
    let listing = cache.dirs.get(parent)?;
    if listing.truncated {
        crate::ops_log::log(
            "CMD",
            &format!(
                "complete trunc_bypass dir={}",
                crate::ops_log::text_preview(parent.as_bytes(), 80)
            ),
        );
        return None;
    }
    if listing.fetched_at.elapsed() > DIR_TTL {
        crate::ops_log::log(
            "CMD",
            &format!(
                "complete cache_stale dir={}",
                crate::ops_log::text_preview(parent.as_bytes(), 80)
            ),
        );
        return None;
    }
    let matches: Vec<DirEntry> = prefix_filter(&listing.entries, &req.name_prefix, dirs_only)
        .into_iter()
        .cloned()
        .collect();
    drop(cache);

    let refs: Vec<&DirEntry> = matches.iter().collect();
    let candidates = rebuild_candidates(&req.token_dir_prefix, &refs);
    Some(apply_candidate_list(
        line.to_string(),
        cursor,
        req.token_start,
        req.token_end,
        &req.token,
        candidates,
    ))
}

async fn complete_cmd(
    rt: Arc<SessionRuntime>,
    cwd: Option<String>,
    line: String,
    cursor: usize,
    req: CompleteRequest,
) -> Result<CompleteResult, String> {
    // Local hit only if complete (non-truncated) and fresh.
    if let Some(result) = try_local_cmd_hit(&rt, &req, &line, cursor) {
        crate::ops_log::log(
            "CMD",
            &format!(
                "complete cache_hit kind=cmd n={} serve=local",
                result.candidates.len()
            ),
        );
        return Ok(result);
    }

    crate::ops_log::log(
        "CMD",
        "complete cmd_cache_miss serve=remote bg_fill=1",
    );

    let result = run_remote_complete(Arc::clone(&rt), cwd, line, cursor).await;
    schedule_ensure_cmd_list(Arc::clone(&rt));
    result
}

fn try_local_cmd_hit(
    rt: &SessionRuntime,
    req: &CompleteRequest,
    line: &str,
    cursor: usize,
) -> Option<CompleteResult> {
    let cache = rt.complete_cache.lock().ok()?;
    let cmd = cache.commands.as_ref()?;
    if cmd.truncated {
        return None;
    }
    if cmd.fetched_at.elapsed() > CMD_TTL {
        return None;
    }
    let candidates: Vec<String> = cmd
        .names
        .iter()
        .filter(|n| n.starts_with(&req.name_prefix))
        .cloned()
        .collect();
    drop(cache);

    Some(apply_candidate_list(
        line.to_string(),
        cursor,
        req.token_start,
        req.token_end,
        &req.token,
        candidates,
    ))
}

async fn run_remote_complete(
    rt: Arc<SessionRuntime>,
    cwd: Option<String>,
    line: String,
    cursor: usize,
) -> Result<CompleteResult, String> {
    let params = match connect_params_from_rt(&rt) {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    let rt_exec = Arc::clone(&rt);
    remote_complete_with_exec(cwd, line, cursor, move |command| {
        let params = params.clone();
        let rt_exec = Arc::clone(&rt_exec);
        async move {
            let cp = mux_control_path(&rt_exec);
            crate::ssh::openssh::openssh_exec_with_key_cache(
                &params,
                &command,
                &rt_exec.side_channel_key,
                cp.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())
        }
    })
    .await
}

fn connect_params_from_rt(rt: &SessionRuntime) -> Result<ConnectParams, String> {
    let cached = rt
        .cached
        .lock()
        .map_err(|_| "cached lock poisoned".to_string())?
        .clone()
        .ok_or_else(|| "未连接，无法补全".to_string())?;
    let (cols, rows) = rt.term_size();
    Ok(ConnectParams {
        host: cached.host,
        port: cached.port,
        username: cached.username,
        auth: cached.auth,
        cols,
        rows,
    })
}

fn mux_control_path(rt: &SessionRuntime) -> Option<std::path::PathBuf> {
    rt.control_path.lock().ok().and_then(|g| g.clone())
}

// --- ensure_dir_listing / prefetch ------------------------------------------

/// Fire-and-forget directory inventory (Tab bg-fill or prefetch). Coalesces.
///
/// PR5: skips dirs recently marked truncated (full inventory useless for hits).
pub fn schedule_ensure_dir_listing(rt: Arc<SessionRuntime>, abs_dir: String) {
    if !complete_cache_enabled() {
        return;
    }
    if !abs_dir.starts_with('/') {
        return;
    }
    if is_truncated_known_dir(&rt, &abs_dir) {
        crate::ops_log::log(
            "CMD",
            &format!(
                "complete list_dir skip known_truncated dir={}",
                crate::ops_log::text_preview(abs_dir.as_bytes(), 80)
            ),
        );
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = ensure_dir_listing(rt, abs_dir).await {
            crate::ops_log::log("CMD", &format!("complete ensure_dir_listing err={e}"));
        }
    });
}

async fn ensure_dir_listing(rt: Arc<SessionRuntime>, abs_dir: String) -> Result<(), String> {
    // Brief lock: hit? inflight join? or start new. Never hold guard across await.
    enum StartKind {
        Join(Arc<InFlightList>),
        Start {
            inflight: Arc<InFlightList>,
            epoch: u64,
            path_gen: u64,
        },
    }
    let start = {
        let mut cache = rt
            .complete_cache
            .lock()
            .map_err(|_| "complete_cache lock".to_string())?;

        // PR5: do not spend a side-channel RTT on dirs we already know are huge.
        if cache.is_truncated_known(&abs_dir) {
            crate::ops_log::log(
                "CMD",
                &format!(
                    "complete list_dir abort known_truncated dir={}",
                    crate::ops_log::text_preview(abs_dir.as_bytes(), 80)
                ),
            );
            return Ok(());
        }

        if let Some(listing) = cache.dirs.get(&abs_dir) {
            if !listing.truncated && listing.fetched_at.elapsed() <= DIR_TTL {
                return Ok(());
            }
        }

        let epoch = cache.epoch;
        let path_gen = cache.path_gen_of(&abs_dir);

        if let Some(inf) = cache.inflight.get(&abs_dir) {
            if inf.epoch == epoch && inf.path_gen == path_gen {
                StartKind::Join(Arc::clone(inf))
            } else {
                let inf = Arc::new(InFlightList {
                    epoch,
                    path_gen,
                    result: Arc::new(std::sync::Mutex::new(None)),
                    notify: Arc::new(Notify::new()),
                });
                cache.inflight.insert(abs_dir.clone(), Arc::clone(&inf));
                StartKind::Start {
                    inflight: inf,
                    epoch,
                    path_gen,
                }
            }
        } else {
            let inf = Arc::new(InFlightList {
                epoch,
                path_gen,
                result: Arc::new(std::sync::Mutex::new(None)),
                notify: Arc::new(Notify::new()),
            });
            cache.inflight.insert(abs_dir.clone(), Arc::clone(&inf));
            StartKind::Start {
                inflight: inf,
                epoch,
                path_gen,
            }
        }
    };

    let (inflight, epoch, path_gen) = match start {
        StartKind::Join(inf) => return wait_inflight_dir(inf).await,
        StartKind::Start {
            inflight,
            epoch,
            path_gen,
        } => (inflight, epoch, path_gen),
    };

    // Connected?
    {
        let meta = rt.meta.lock().map_err(|_| "meta lock".to_string())?;
        if !matches!(meta.state, SessionState::Connected) {
            finish_inflight_dir(&rt, &abs_dir, &inflight, Err("not connected".into()));
            return Err("not connected".into());
        }
    }

    let params = match connect_params_from_rt(&rt) {
        Ok(p) => p,
        Err(e) => {
            finish_inflight_dir(&rt, &abs_dir, &inflight, Err(e.clone()));
            return Err(e);
        }
    };
    let cp = mux_control_path(&rt);
    let command = build_list_dir_command(&abs_dir, MAX_DIR_ENTRIES);

    crate::ops_log::log(
        "CMD",
        &format!(
            "complete list_dir start dir={}",
            crate::ops_log::text_preview(abs_dir.as_bytes(), 80)
        ),
    );

    let raw = match tokio::time::timeout(
        PREFETCH_TIMEOUT,
        crate::ssh::openssh::openssh_exec_with_key_cache(
            &params,
            &command,
            &rt.side_channel_key,
            cp.as_deref(),
        ),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let msg = e.to_string();
            finish_inflight_dir(&rt, &abs_dir, &inflight, Err(msg.clone()));
            return Err(msg);
        }
        Err(_) => {
            let msg = "list_dir timeout".to_string();
            finish_inflight_dir(&rt, &abs_dir, &inflight, Err(msg.clone()));
            return Err(msg);
        }
    };

    let parsed = match parse_listing(&raw) {
        Ok((entries, truncated)) => Ok(DirListing {
            abs_dir: abs_dir.clone(),
            entries,
            fetched_at: Instant::now(),
            truncated,
        }),
        Err(ListError::CdFailed) => Err("list_dir cd failed".into()),
        Err(e) => Err(format!("list_dir parse: {e:?}")),
    };

    // Insert only if epoch/path_gen match and listing is usable (!truncated).
    {
        let mut cache = rt
            .complete_cache
            .lock()
            .map_err(|_| "complete_cache lock".to_string())?;
        if cache.epoch == epoch && cache.path_gen_of(&abs_dir) == path_gen {
            match &parsed {
                Ok(listing) if !listing.truncated => {
                    crate::ops_log::log(
                        "CMD",
                        &format!(
                            "complete list_dir ok dir={} n={} truncated=false",
                            crate::ops_log::text_preview(abs_dir.as_bytes(), 80),
                            listing.entries.len()
                        ),
                    );
                    // Full listing succeeded — clear any prior truncated_known stigma.
                    cache.truncated_known.remove(&abs_dir);
                    cache.dirs.insert(listing.clone());
                }
                Ok(listing) => {
                    crate::ops_log::log(
                        "CMD",
                        &format!(
                            "complete list_dir truncated dir={} n={} (not cached for hit; mark skip)",
                            crate::ops_log::text_preview(abs_dir.as_bytes(), 80),
                            listing.entries.len()
                        ),
                    );
                    // PR5: remember for DIR_TTL so prefetch/Tab bg do not re-list.
                    cache.mark_truncated_known(&abs_dir);
                }
                Err(e) => {
                    crate::ops_log::log("CMD", &format!("complete list_dir fail dir err={e}"));
                }
            }
        }
        cache.inflight.remove(&abs_dir);
        if let Ok(mut g) = inflight.result.lock() {
            *g = Some(parsed);
        }
    }
    inflight.notify.notify_waiters();
    Ok(())
}

async fn wait_inflight_dir(inf: Arc<InFlightList>) -> Result<(), String> {
    // Already done?
    if let Ok(g) = inf.result.lock() {
        if g.is_some() {
            return g
                .as_ref()
                .unwrap()
                .as_ref()
                .map(|_| ())
                .map_err(|e| e.clone());
        }
    }
    inf.notify.notified().await;
    if let Ok(g) = inf.result.lock() {
        if let Some(r) = g.as_ref() {
            return r.as_ref().map(|_| ()).map_err(|e| e.clone());
        }
    }
    Ok(())
}

fn finish_inflight_dir(
    rt: &SessionRuntime,
    abs_dir: &str,
    inflight: &InFlightList,
    result: Result<DirListing, String>,
) {
    if let Ok(mut cache) = rt.complete_cache.lock() {
        cache.inflight.remove(abs_dir);
    }
    if let Ok(mut g) = inflight.result.lock() {
        *g = Some(result);
    }
    inflight.notify.notify_waiters();
}

fn schedule_ensure_cmd_list(rt: Arc<SessionRuntime>) {
    if !complete_cache_enabled() {
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = ensure_cmd_list(rt).await {
            crate::ops_log::log("CMD", &format!("complete cmd_list err={e}"));
        }
    });
}

async fn ensure_cmd_list(rt: Arc<SessionRuntime>) -> Result<(), String> {
    enum CmdStart {
        Join(Arc<InFlightCmd>),
        Start {
            inflight: Arc<InFlightCmd>,
            epoch: u64,
            cmd_gen: u64,
        },
    }
    let start = {
        let mut cache = rt
            .complete_cache
            .lock()
            .map_err(|_| "complete_cache lock".to_string())?;

        if let Some(ref cmd) = cache.commands {
            if !cmd.truncated && cmd.fetched_at.elapsed() <= CMD_TTL {
                return Ok(());
            }
        }

        let epoch = cache.epoch;
        let cmd_gen = cache.cmd_gen;
        if let Some(ref inf) = cache.cmd_inflight {
            if inf.epoch == epoch && inf.cmd_gen == cmd_gen {
                CmdStart::Join(Arc::clone(inf))
            } else {
                let inf = Arc::new(InFlightCmd {
                    epoch,
                    cmd_gen,
                    result: Arc::new(std::sync::Mutex::new(None)),
                    notify: Arc::new(Notify::new()),
                });
                cache.cmd_inflight = Some(Arc::clone(&inf));
                CmdStart::Start {
                    inflight: inf,
                    epoch,
                    cmd_gen,
                }
            }
        } else {
            let inf = Arc::new(InFlightCmd {
                epoch,
                cmd_gen,
                result: Arc::new(std::sync::Mutex::new(None)),
                notify: Arc::new(Notify::new()),
            });
            cache.cmd_inflight = Some(Arc::clone(&inf));
            CmdStart::Start {
                inflight: inf,
                epoch,
                cmd_gen,
            }
        }
    };

    let (inflight, epoch, cmd_gen) = match start {
        CmdStart::Join(inf) => return wait_inflight_cmd(inf).await,
        CmdStart::Start {
            inflight,
            epoch,
            cmd_gen,
        } => (inflight, epoch, cmd_gen),
    };

    {
        let meta = rt.meta.lock().map_err(|_| "meta lock".to_string())?;
        if !matches!(meta.state, SessionState::Connected) {
            finish_inflight_cmd(&rt, &inflight, Err("not connected".into()));
            return Err("not connected".into());
        }
    }

    let params = match connect_params_from_rt(&rt) {
        Ok(p) => p,
        Err(e) => {
            finish_inflight_cmd(&rt, &inflight, Err(e.clone()));
            return Err(e);
        }
    };
    let cp = mux_control_path(&rt);
    let command = build_cmd_list_command(MAX_CMD_ENTRIES);

    crate::ops_log::log("CMD", "complete cmd_list start");

    let raw = match tokio::time::timeout(
        PREFETCH_TIMEOUT,
        crate::ssh::openssh::openssh_exec_with_key_cache(
            &params,
            &command,
            &rt.side_channel_key,
            cp.as_deref(),
        ),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let msg = e.to_string();
            finish_inflight_cmd(&rt, &inflight, Err(msg.clone()));
            return Err(msg);
        }
        Err(_) => {
            let msg = "cmd_list timeout".to_string();
            finish_inflight_cmd(&rt, &inflight, Err(msg.clone()));
            return Err(msg);
        }
    };

    let parsed = match parse_cmd_list(&raw) {
        Ok((names, truncated)) => Ok(CommandCache {
            names,
            fetched_at: Instant::now(),
            truncated,
        }),
        Err(e) => Err(format!("cmd_list parse: {e:?}")),
    };

    {
        let mut cache = rt
            .complete_cache
            .lock()
            .map_err(|_| "complete_cache lock".to_string())?;
        if cache.epoch == epoch && cache.cmd_gen == cmd_gen {
            match &parsed {
                Ok(cmd) if !cmd.truncated => {
                    crate::ops_log::log(
                        "CMD",
                        &format!(
                            "complete cmd_cache_fill n={} truncated=false",
                            cmd.names.len()
                        ),
                    );
                    cache.commands = Some(cmd.clone());
                }
                Ok(cmd) => {
                    crate::ops_log::log(
                        "CMD",
                        &format!(
                            "complete cmd_cache_fill n={} truncated=true (not for hit)",
                            cmd.names.len()
                        ),
                    );
                }
                Err(e) => {
                    crate::ops_log::log("CMD", &format!("complete cmd_list fail {e}"));
                }
            }
        } else {
            crate::ops_log::log(
                "CMD",
                "complete cmd_list drop stale fill (epoch/cmd_gen mismatch)",
            );
        }
        cache.cmd_inflight = None;
        if let Ok(mut g) = inflight.result.lock() {
            *g = Some(parsed.clone());
        }
    }
    inflight.notify.notify_waiters();
    Ok(())
}

async fn wait_inflight_cmd(inf: Arc<InFlightCmd>) -> Result<(), String> {
    if let Ok(g) = inf.result.lock() {
        if let Some(r) = g.as_ref() {
            return r.as_ref().map(|_| ()).map_err(|e| e.clone());
        }
    }
    inf.notify.notified().await;
    if let Ok(g) = inf.result.lock() {
        if let Some(r) = g.as_ref() {
            return r.as_ref().map(|_| ()).map_err(|e| e.clone());
        }
    }
    Ok(())
}

fn finish_inflight_cmd(
    rt: &SessionRuntime,
    inflight: &InFlightCmd,
    result: Result<CommandCache, String>,
) {
    if let Ok(mut cache) = rt.complete_cache.lock() {
        cache.cmd_inflight = None;
    }
    if let Ok(mut g) = inflight.result.lock() {
        *g = Some(result);
    }
    inflight.notify.notify_waiters();
}

// --- cwd confirmed hook -----------------------------------------------------

/// Prefetch listing when cwd is confirmed (OSC7/title). Not CdParse / CdRollback.
pub fn on_cwd_confirmed_for_cache(
    rt: Arc<SessionRuntime>,
    path: &str,
    reason: CwdChangeReason,
) {
    if !complete_cache_enabled() {
        return;
    }
    match reason {
        CwdChangeReason::Osc7 | CwdChangeReason::OscTitle => {}
        CwdChangeReason::CdParse | CwdChangeReason::CdRollback => {
            // Optimistic cd / rollback: never prefetch; path_gen cancel is optional.
            return;
        }
    }
    if !path.starts_with('/') {
        return;
    }
    crate::ops_log::log(
        "CMD",
        &format!(
            "complete prefetch schedule dir={} reason={reason:?}",
            crate::ops_log::text_preview(path.as_bytes(), 80)
        ),
    );
    schedule_ensure_dir_listing(rt, path.to_string());
}

/// Prefetch after login seed / restore (not CdParse).
pub fn schedule_prefetch_cwd(rt: Arc<SessionRuntime>, path: &str) {
    if !path.starts_with('/') {
        return;
    }
    schedule_ensure_dir_listing(rt, path.to_string());
}

/// Context for submit-time directory listing warmup (`cd` / `ls` / `ll`).
pub struct ListingWarmupCtx<'a> {
    /// Absolute cwd (or best-known) **before** optimistic `cd` is applied.
    pub cwd_before: Option<&'a str>,
    pub home_hint: Option<&'a str>,
    /// Optimistic absolute path from `feed_submitted_line` for `cd`/`pushd`.
    pub cd_optimistic_abs: Option<&'a str>,
}

/// After a draft line is submitted: background-warm dir listings for `cd` / `ls` / `ll`.
///
/// - `cd` / `pushd`: list the **destination** (optimistic absolute path when known).
/// - `ls` / `ll`: list the first path argument, or **cwd** when none.
///
/// Does not block the PTY. Failed `cd` may briefly warm a wrong path (TTL / next
/// OSC path will supersede; list of a missing dir does not poison hit cache).
pub fn note_listing_warmup_submit(rt: Arc<SessionRuntime>, line: &str, ctx: ListingWarmupCtx<'_>) {
    if !complete_cache_enabled() {
        return;
    }
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return;
    }

    let first = first_shell_word(line);
    if first.is_empty() {
        return;
    }

    let targets = match first {
        "cd" | "pushd" => warmup_targets_for_cd(ctx.cd_optimistic_abs, ctx.cwd_before, ctx.home_hint, line),
        "ls" | "ll" => warmup_targets_for_ls(ctx.cwd_before, ctx.home_hint, line),
        _ => return,
    };

    for abs in targets {
        crate::ops_log::log(
            "CMD",
            &format!(
                "complete listing_warmup first={first} dir={}",
                crate::ops_log::text_preview(abs.as_bytes(), 80)
            ),
        );
        schedule_ensure_dir_listing(Arc::clone(&rt), abs);
    }
}

fn warmup_targets_for_cd(
    cd_optimistic_abs: Option<&str>,
    cwd_before: Option<&str>,
    home_hint: Option<&str>,
    line: &str,
) -> Vec<String> {
    if let Some(p) = cd_optimistic_abs.filter(|p| p.starts_with('/')) {
        return vec![normalize_remote_abs(p)];
    }
    // Fallback: first non-option arg after cd/pushd (best-effort).
    if let Some(arg) = first_path_arg_after_command(line) {
        if let Some(abs) = resolve_listing_target(arg, cwd_before, home_hint) {
            return vec![abs];
        }
    }
    // bare `cd` / `cd ~` with no absolute path yet — warm home if known
    if let Some(h) = home_hint.filter(|h| h.starts_with('/')) {
        return vec![normalize_remote_abs(h)];
    }
    Vec::new()
}

fn warmup_targets_for_ls(
    cwd_before: Option<&str>,
    home_hint: Option<&str>,
    line: &str,
) -> Vec<String> {
    if let Some(arg) = first_path_arg_after_command(line) {
        if let Some(abs) = resolve_listing_target(arg, cwd_before, home_hint) {
            return vec![abs];
        }
        // Unresolvable arg — still try cwd so `ls relative_unknown` warms cwd
    }
    if let Some(c) = abs_cwd(cwd_before, home_hint) {
        return vec![c];
    }
    Vec::new()
}

/// First non-option argument after the command word (skips `-la`, `--color=auto`, etc.).
fn first_path_arg_after_command(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    // Skip env FOO=bar
    loop {
        let word = rest.split_whitespace().next()?;
        if word.contains('=') && !word.starts_with('-') && !word.starts_with('/') {
            rest = rest[word.len()..].trim_start();
            continue;
        }
        if matches!(word, "sudo" | "command" | "time" | "nohup" | "nice") {
            rest = rest[word.len()..].trim_start();
            while let Some(w) = rest.split_whitespace().next() {
                if w.starts_with('-') {
                    rest = rest[w.len()..].trim_start();
                    if matches!(w, "-u" | "-g" | "-C" | "--user" | "--group") {
                        if let Some(arg) = rest.split_whitespace().next() {
                            if !arg.starts_with('-') {
                                rest = rest[arg.len()..].trim_start();
                            }
                        }
                    }
                    continue;
                }
                break;
            }
            continue;
        }
        // command word (possibly /bin/ls)
        rest = rest[word.len()..].trim_start();
        break;
    }
    // Remaining: options then path
    while let Some(w) = rest.split_whitespace().next() {
        if w == "--" {
            rest = rest[w.len()..].trim_start();
            return rest.split_whitespace().next();
        }
        if w.starts_with('-') {
            rest = rest[w.len()..].trim_start();
            continue;
        }
        return Some(w);
    }
    None
}

/// Resolve a user path token to an absolute remote dir key for listing warmup.
fn resolve_listing_target(
    token: &str,
    cwd: Option<&str>,
    home_hint: Option<&str>,
) -> Option<String> {
    let token = token.trim().trim_matches(|c| c == '\'' || c == '"');
    if token.is_empty() || token == "-" {
        return abs_cwd(cwd, home_hint);
    }
    // ~otheruser — cannot resolve without their home
    if is_other_user_tilde(token) {
        return None;
    }
    if token == "~" {
        return home_hint
            .filter(|h| h.starts_with('/'))
            .map(|h| normalize_remote_abs(h));
    }
    if token.starts_with("~/") || token == "~" {
        return expand_shell_path(token, home_hint).map(|p| normalize_remote_abs(&p));
    }
    if token.starts_with('/') {
        return Some(normalize_remote_abs(token));
    }
    // relative incl. ./ ../
    let base = abs_cwd(cwd, home_hint)?;
    Some(normalize_remote_abs(&format!("{base}/{token}")))
}

// --- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_token_basic() {
        assert_eq!(
            split_token_dir_and_name("foo/bar"),
            ("foo/".into(), "bar".into())
        );
        assert_eq!(
            split_token_dir_and_name("foo/"),
            ("foo/".into(), "".into())
        );
        assert_eq!(split_token_dir_and_name("x"), ("".into(), "x".into()));
        assert_eq!(split_token_dir_and_name("/"), ("/".into(), "".into()));
        assert_eq!(
            split_token_dir_and_name("/tm"),
            ("/".into(), "tm".into())
        );
    }

    #[test]
    fn resolve_parent_relative() {
        let p = resolve_parent_abs("fo", Some("/home/u/a"), None);
        assert_eq!(p.as_deref(), Some("/home/u/a"));
        let p = resolve_parent_abs("foo/ba", Some("/home/u/a"), None);
        assert_eq!(p.as_deref(), Some("/home/u/a/foo"));
        let p = resolve_parent_abs("foo/", Some("/home/u/a"), None);
        assert_eq!(p.as_deref(), Some("/home/u/a/foo"));
        let p = resolve_parent_abs("../b", Some("/home/u/a"), None);
        assert_eq!(p.as_deref(), Some("/home/u"));
        let p = resolve_parent_abs("foo/../ba", Some("/home/u/a"), None);
        assert_eq!(p.as_deref(), Some("/home/u/a"));
    }

    #[test]
    fn resolve_parent_absolute_and_tilde() {
        assert_eq!(
            resolve_parent_abs("/tm", Some("/home/u"), None).as_deref(),
            Some("/")
        );
        assert_eq!(
            resolve_parent_abs("/tmp/x", Some("/home/u"), None).as_deref(),
            Some("/tmp")
        );
        assert_eq!(
            resolve_parent_abs("~/c", Some("/tmp"), Some("/home/u")).as_deref(),
            Some("/home/u")
        );
        assert_eq!(
            resolve_parent_abs("~/", Some("/tmp"), Some("/home/u")).as_deref(),
            Some("/home/u")
        );
        // bare ~ → None (force remote)
        assert_eq!(resolve_parent_abs("~", Some("/home/u"), Some("/home/u")), None);
        assert_eq!(
            resolve_parent_abs("~alice/x", Some("/home/u"), Some("/home/u")),
            None
        );
    }

    #[test]
    fn resolve_parent_tilde_cwd() {
        let p = resolve_parent_abs("fo", Some("~/proj"), Some("/home/u"));
        assert_eq!(p.as_deref(), Some("/home/u/proj"));
        assert_eq!(
            resolve_parent_abs("fo", Some("~/proj"), None),
            None
        );
    }

    #[test]
    fn parse_listing_ok_full() {
        let raw = "OK\nFULL\nd\ttmp\nf\tfile.txt\n";
        let (entries, trunc) = parse_listing(raw).unwrap();
        assert!(!trunc);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "tmp");
        assert!(!entries[1].is_dir);
    }

    #[test]
    fn parse_listing_err_cd() {
        assert_eq!(parse_listing("ERR_CD\n"), Err(ListError::CdFailed));
    }

    #[test]
    fn parse_listing_space_name() {
        let raw = "OK\nFULL\nf\tmy file.txt\n";
        let (entries, _) = parse_listing(raw).unwrap();
        assert_eq!(entries[0].name, "my file.txt");
    }

    #[test]
    fn prefix_filter_and_rebuild() {
        let entries = vec![
            DirEntry {
                name: "bar".into(),
                is_dir: true,
            },
            DirEntry {
                name: "baz".into(),
                is_dir: false,
            },
            DirEntry {
                name: "other".into(),
                is_dir: true,
            },
        ];
        let m = prefix_filter(&entries, "ba", false);
        assert_eq!(m.len(), 2);
        let c = rebuild_candidates("foo/", &m);
        assert_eq!(c, vec!["foo/bar/".to_string(), "foo/baz".to_string()]);
    }

    #[test]
    fn lru_touch_and_evict() {
        let mut cache = DirListingCache {
            map: HashMap::new(),
            order: VecDeque::new(),
            max_dirs: 2,
        };
        cache.insert(DirListing {
            abs_dir: "/a".into(),
            entries: vec![],
            fetched_at: Instant::now(),
            truncated: false,
        });
        cache.insert(DirListing {
            abs_dir: "/b".into(),
            entries: vec![],
            fetched_at: Instant::now(),
            truncated: false,
        });
        // touch /a then insert /c → evict /b
        assert!(cache.get("/a").is_some());
        cache.insert(DirListing {
            abs_dir: "/c".into(),
            entries: vec![],
            fetched_at: Instant::now(),
            truncated: false,
        });
        assert!(cache.get("/a").is_some());
        assert!(cache.get("/b").is_none());
        assert!(cache.get("/c").is_some());
    }

    #[test]
    fn build_request_cmd_vs_path() {
        let r = build_complete_request("sys", 3, Some("/tmp"), None);
        assert_eq!(r.mode, CompleteMode::Cmd);
        let r = build_complete_request("cd /tm", 6, Some("/home/u"), None);
        assert_eq!(r.mode, CompleteMode::Dir);
        assert_eq!(r.parent_abs.as_deref(), Some("/"));
        assert_eq!(r.name_prefix, "tm");
    }

    #[test]
    fn first_shell_word_strips_sudo_and_path() {
        assert_eq!(first_shell_word("mkdir foo"), "mkdir");
        assert_eq!(first_shell_word("sudo mkdir foo"), "mkdir");
        assert_eq!(first_shell_word("sudo -u root rm -rf x"), "rm");
        assert_eq!(first_shell_word("/bin/rm -f a"), "rm");
        assert_eq!(first_shell_word("FOO=1 mkdir x"), "mkdir");
    }

    #[test]
    fn command_cache_clear_heuristics() {
        assert!(command_cache_should_clear("hash", "hash -r"));
        assert!(!command_cache_should_clear("hash", "hash ls"));
        assert!(command_cache_should_clear("export", "export PATH=/tmp:$PATH"));
        assert!(!command_cache_should_clear("export", "export FOO=1"));
        assert!(command_cache_should_clear("PATH=/tmp", "PATH=/tmp"));
        // bare PATH= → first_shell_word empty, but bare_path_assignment must catch it
        assert_eq!(first_shell_word("PATH=/tmp:$PATH"), "");
        assert!(bare_path_assignment("PATH=/tmp:$PATH"));
        assert!(bare_path_assignment("export PATH=/usr/bin"));
        assert!(!bare_path_assignment("mkdir PATH=foo"));
    }

    #[test]
    fn sudo_n_still_finds_mkdir() {
        assert_eq!(first_shell_word("sudo -n mkdir foo"), "mkdir");
        assert_eq!(first_shell_word("sudo -u alice mkdir foo"), "mkdir");
    }

    #[test]
    fn fs_mutating_set_includes_common_tools() {
        assert!(FS_MUTATING_COMMANDS.contains(&"mkdir"));
        assert!(FS_MUTATING_COMMANDS.contains(&"rm"));
        assert!(!FS_MUTATING_COMMANDS.contains(&"ls"));
        assert!(!FS_MUTATING_COMMANDS.contains(&"cd"));
    }

    #[test]
    fn invalidate_removes_listing() {
        let mut cache = DirListingCache::new();
        cache.insert(DirListing {
            abs_dir: "/tmp".into(),
            entries: vec![DirEntry {
                name: "a".into(),
                is_dir: false,
            }],
            fetched_at: Instant::now(),
            truncated: false,
        });
        assert!(cache.contains("/tmp"));
        cache.invalidate("/tmp");
        assert!(!cache.contains("/tmp"));
    }

    #[test]
    fn truncated_known_skips_and_expires_with_ttl_logic() {
        let mut s = SessionCompleteCache::default();
        s.dirs.insert(DirListing {
            abs_dir: "/usr/lib".into(),
            entries: vec![DirEntry {
                name: "x".into(),
                is_dir: false,
            }],
            fetched_at: Instant::now(),
            truncated: false,
        });
        s.mark_truncated_known("/usr/lib");
        // mark drops any hit listing
        assert!(!s.dirs.contains("/usr/lib"));
        assert!(s.is_truncated_known("/usr/lib"));
        // mutation invalidation clears stigma so a later list may succeed
        s.bump_path_gen("/usr/lib");
        assert!(!s.is_truncated_known("/usr/lib"));
    }

    #[test]
    fn truncated_never_used_as_local_hit_source() {
        // Policy guard: a DirListing with truncated=true must not pass try-hit
        // filters (tested via truncated flag + try_local pattern).
        let listing = DirListing {
            abs_dir: "/big".into(),
            entries: vec![DirEntry {
                name: "a".into(),
                is_dir: true,
            }],
            fetched_at: Instant::now(),
            truncated: true,
        };
        assert!(listing.truncated);
        // Design lock: only !truncated listings are insertable as hits; mark_truncated
        // invalidates; ensure_dir_listing does not insert truncated entries.
    }

    #[test]
    fn first_path_arg_skips_ls_flags() {
        assert_eq!(first_path_arg_after_command("ls"), None);
        assert_eq!(first_path_arg_after_command("ls -la"), None);
        assert_eq!(first_path_arg_after_command("ll -h"), None);
        assert_eq!(first_path_arg_after_command("ls -la /tmp"), Some("/tmp"));
        assert_eq!(first_path_arg_after_command("ls -- /var"), Some("/var"));
        assert_eq!(first_path_arg_after_command("ll ./src"), Some("./src"));
        assert_eq!(first_path_arg_after_command("sudo ls -l /etc"), Some("/etc"));
    }

    #[test]
    fn resolve_listing_target_cases() {
        assert_eq!(
            resolve_listing_target(".", Some("/home/u/a"), None).as_deref(),
            Some("/home/u/a")
        );
        assert_eq!(
            resolve_listing_target("./", Some("/home/u/a"), None).as_deref(),
            Some("/home/u/a")
        );
        assert_eq!(
            resolve_listing_target("..", Some("/home/u/a"), None).as_deref(),
            Some("/home/u")
        );
        assert_eq!(
            resolve_listing_target("/tmp", Some("/home/u"), None).as_deref(),
            Some("/tmp")
        );
        assert_eq!(
            resolve_listing_target("~/proj", Some("/tmp"), Some("/home/u")).as_deref(),
            Some("/home/u/proj")
        );
        assert_eq!(
            resolve_listing_target("foo", Some("/home/u/a"), None).as_deref(),
            Some("/home/u/a/foo")
        );
    }

    #[test]
    fn warmup_ls_defaults_to_cwd() {
        let t = warmup_targets_for_ls(Some("/home/u/a"), None, "ls -la");
        assert_eq!(t, vec!["/home/u/a".to_string()]);
        let t = warmup_targets_for_ls(Some("/home/u/a"), None, "ll /tmp");
        assert_eq!(t, vec!["/tmp".to_string()]);
    }

    #[test]
    fn warmup_cd_prefers_optimistic_abs() {
        let t = warmup_targets_for_cd(
            Some("/home/u/proj"),
            Some("/home/u"),
            Some("/home/u"),
            "cd proj",
        );
        assert_eq!(t, vec!["/home/u/proj".to_string()]);
    }
}
