//! Remote Tab completion for the draft bar (Xshell-like).
//!
//! Uses a short-lived non-interactive `ssh host 'script'` (or russh exec) so the
//! interactive PTY is not polluted. Prefers bash `compgen`; falls back to a
//! simple listing when bash completion helpers are unavailable.

use std::future::Future;
use std::time::Duration;

use base64::Engine;
use serde::Serialize;
use tracing::debug;

const COMPLETE_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_CANDIDATES: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct CompleteResult {
    /// Full draft line after applying common-prefix / single match.
    pub line: String,
    /// Cursor position (char index) after applying completion.
    pub cursor: usize,
    /// All matches for the current token (may be many).
    pub candidates: Vec<String>,
    /// Start index (chars) of the token replaced in the original line.
    pub token_start: usize,
    /// End index (chars) of the token replaced in the original line (exclusive).
    pub token_end: usize,
}

/// Run remote completion via any one-shot exec transport (OpenSSH `ssh host cmd`).
pub async fn remote_complete_with_exec<F, Fut>(
    cwd: Option<String>,
    line: String,
    cursor: usize,
    exec: F,
) -> Result<CompleteResult, String>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    let chars: Vec<char> = line.chars().collect();
    let cursor = cursor.min(chars.len());
    let (token_start, token_end, token, _head, is_first_word, prefer_dirs) =
        analyze_token(&chars, cursor);

    let cwd = cwd.unwrap_or_else(|| ".".into());
    let command = build_exec_command(&cwd, &token, is_first_word, prefer_dirs);

    debug!(
        token = %token,
        is_first_word,
        prefer_dirs,
        cwd = %cwd,
        "remote complete"
    );

    let raw = match tokio::time::timeout(COMPLETE_TIMEOUT, exec(command)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("补全超时，请重试".into()),
    };

    apply_candidates(line, cursor, token_start, token_end, &token, &raw)
}

fn apply_candidates(
    line: String,
    cursor: usize,
    token_start: usize,
    token_end: usize,
    token: &str,
    raw: &str,
) -> Result<CompleteResult, String> {
    let chars: Vec<char> = line.chars().collect();
    let mut candidates = parse_candidates(raw);
    candidates.sort();
    candidates.dedup();
    if candidates.len() > MAX_CANDIDATES {
        candidates.truncate(MAX_CANDIDATES);
    }

    if candidates.is_empty() {
        return Ok(CompleteResult {
            line,
            cursor,
            candidates,
            token_start,
            token_end,
        });
    }

    let common = common_prefix(&candidates);
    let applied = if candidates.len() == 1 {
        candidates[0].clone()
    } else if common.chars().count() > token.chars().count() {
        common
    } else {
        // Multiple matches, no longer common prefix — keep token, return list.
        token.to_string()
    };

    let mut new_chars: Vec<char> = Vec::with_capacity(chars.len() + applied.chars().count());
    new_chars.extend_from_slice(&chars[..token_start]);
    new_chars.extend(applied.chars());
    new_chars.extend_from_slice(&chars[token_end..]);
    let new_cursor = token_start + applied.chars().count();
    let new_line: String = new_chars.into_iter().collect();

    Ok(CompleteResult {
        line: new_line,
        cursor: new_cursor,
        candidates,
        token_start,
        token_end,
    })
}

/// Locate the token under/before the cursor (whitespace-separated; no quote parse v1).
fn analyze_token(chars: &[char], cursor: usize) -> (usize, usize, String, String, bool, bool) {
    let mut start = cursor;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let token: String = chars[start..cursor].iter().collect();
    let head: String = chars[..start].iter().collect();
    let head_trim = head.trim();
    let is_first_word = head_trim.is_empty();

    let first_cmd = head_trim.split_whitespace().next().unwrap_or("");
    let prefer_dirs = matches!(first_cmd, "cd" | "pushd" | "rmdir");

    (start, cursor, token, head, is_first_word, prefer_dirs)
}

/// Build: `echo SCRIPT_B64 | base64 -d | bash --noprofile --norc`
/// so we never nest shell quotes around user-controlled paths.
fn build_exec_command(cwd: &str, token: &str, is_first_word: bool, prefer_dirs: bool) -> String {
    let mode = if is_first_word {
        "cmd"
    } else if prefer_dirs {
        "dir"
    } else {
        "file"
    };

    // Script reads CWD/TOKEN/MODE as base64 constants baked in (also base64-safe).
    let script = format!(
        r#"set +e
b64d() {{
  if command -v base64 >/dev/null 2>&1; then
    printf '%s' "$1" | base64 -d 2>/dev/null || printf '%s' "$1" | base64 --decode 2>/dev/null
  elif command -v openssl >/dev/null 2>&1; then
    printf '%s' "$1" | openssl base64 -d -A 2>/dev/null
  else
    printf ''
  fi
}}
CWD=$(b64d '{cwd_b64}')
TOKEN=$(b64d '{token_b64}')
MODE='{mode}'
cd -- "$CWD" 2>/dev/null || true
out=''
if command -v compgen >/dev/null 2>&1; then
  case "$MODE" in
    cmd)
      out=$( {{ compgen -c -- "$TOKEN"; compgen -a -- "$TOKEN"; compgen -A function -- "$TOKEN"; }} 2>/dev/null)
      ;;
    dir)
      out=$(compgen -d -- "$TOKEN" 2>/dev/null)
      ;;
    *)
      out=$(compgen -f -- "$TOKEN" 2>/dev/null)
      ;;
  esac
else
  case "$MODE" in
    cmd)
      out=$(printf '%s\n' ls cd pwd cat cp mv rm mkdir touch grep find echo head tail less more vim nano vi ssh scp tar gzip unzip wget curl docker git python python3 node npm cargo go make)
      ;;
    dir)
      out=$(ls -1d "$TOKEN"* 2>/dev/null | while IFS= read -r p; do [ -d "$p" ] && printf '%s\n' "$p"; done)
      ;;
    *)
      out=$(ls -1d "$TOKEN"* 2>/dev/null)
      ;;
  esac
fi
printf '%s\n' "$out" | awk 'NF' | head -n {max} | while IFS= read -r c; do
  if [ -d "$c" ]; then
    case "$c" in
      */) printf '%s\n' "$c" ;;
      *)  printf '%s/\n' "$c" ;;
    esac
  else
    printf '%s\n' "$c"
  fi
done
exit 0
"#,
        cwd_b64 = b64(cwd.as_bytes()),
        token_b64 = b64(token.as_bytes()),
        mode = mode,
        max = MAX_CANDIDATES,
    );

    let script_b64 = b64(script.as_bytes());
    // Prefer base64 -d; some systems use -D or openssl.
    format!(
        "echo {script_b64} | (base64 -d 2>/dev/null || base64 --decode 2>/dev/null || openssl base64 -d -A 2>/dev/null) | bash --noprofile --norc"
    )
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn parse_candidates(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|l| l.trim_end_matches(['\r', '\n']).trim())
        .filter(|l| !l.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn common_prefix(items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut prefix: Vec<char> = items[0].chars().collect();
    for s in items.iter().skip(1) {
        let chars: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < prefix.len() && i < chars.len() && prefix[i] == chars[i] {
            i += 1;
        }
        prefix.truncate(i);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_first_word() {
        let chars: Vec<char> = "sys".chars().collect();
        let (start, end, token, _, first, dirs) = analyze_token(&chars, 3);
        assert_eq!(start, 0);
        assert_eq!(end, 3);
        assert_eq!(token, "sys");
        assert!(first);
        assert!(!dirs);
    }

    #[test]
    fn token_path_after_cd() {
        let chars: Vec<char> = "cd /tm".chars().collect();
        let (start, end, token, _, first, dirs) = analyze_token(&chars, 6);
        assert_eq!(start, 3);
        assert_eq!(end, 6);
        assert_eq!(token, "/tm");
        assert!(!first);
        assert!(dirs);
    }

    #[test]
    fn common_prefix_works() {
        let items = vec!["tmp/a".into(), "tmp/b".into(), "tmp/c".into()];
        assert_eq!(common_prefix(&items), "tmp/");
    }

    #[test]
    fn exec_command_is_plain_ascii() {
        let cmd = build_exec_command("/tmp", "sys", true, false);
        assert!(cmd.starts_with("echo "));
        assert!(cmd.contains("bash --noprofile --norc"));
    }
}
