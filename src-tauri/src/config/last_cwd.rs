//! Persist last known absolute cwd per host+user.
//!
//! Survives tab close / app restart so the next connect to the same endpoint
//! can run the restore playbook (same as in-memory `restore_target`).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::config_dir;
use crate::error::AppError;

const MAX_ENTRIES: usize = 80;

#[derive(Debug, Default, Serialize, Deserialize)]
struct LastCwdStore {
    /// key = `user@host` (host lowercased) → absolute path
    #[serde(default)]
    paths: HashMap<String, String>,
    /// Insertion/update order for LRU eviction (newest last).
    #[serde(default)]
    order: Vec<String>,
}

fn store_path() -> Result<PathBuf, AppError> {
    Ok(config_dir()?.join("last_cwd.json"))
}

pub fn endpoint_key(host: &str, username: &str) -> String {
    format!("{}@{}", username.trim(), host.trim().to_ascii_lowercase())
}

fn load_store() -> Result<LastCwdStore, AppError> {
    let path = store_path()?;
    if !path.exists() {
        return Ok(LastCwdStore::default());
    }
    let raw = fs::read_to_string(&path)?;
    if raw.trim().is_empty() {
        return Ok(LastCwdStore::default());
    }
    let store: LastCwdStore = serde_json::from_str(&raw).unwrap_or_default();
    Ok(store)
}

fn save_store(store: &LastCwdStore) -> Result<(), AppError> {
    let path = store_path()?;
    let raw = serde_json::to_string_pretty(store)?;
    fs::write(path, raw)?;
    Ok(())
}

/// Load persisted absolute cwd for host+user, if any.
pub fn load_last_cwd(host: &str, username: &str) -> Option<String> {
    if host.trim().is_empty() || username.trim().is_empty() {
        return None;
    }
    let key = endpoint_key(host, username);
    let store = load_store().ok()?;
    let path = store.paths.get(&key)?.clone();
    if path.starts_with('/') {
        Some(path)
    } else {
        None
    }
}

/// Remember absolute cwd for host+user (overwrite previous).
pub fn save_last_cwd(host: &str, username: &str, path: &str) {
    if host.trim().is_empty() || username.trim().is_empty() {
        return;
    }
    if !path.starts_with('/') {
        return;
    }
    let key = endpoint_key(host, username);
    let mut store = match load_store() {
        Ok(s) => s,
        Err(e) => {
            crate::ops_log::log("ERR", &format!("last_cwd load failed: {e}"));
            return;
        }
    };
    store.paths.insert(key.clone(), path.to_string());
    store.order.retain(|k| k != &key);
    store.order.push(key);
    while store.order.len() > MAX_ENTRIES {
        if let Some(old) = store.order.first().cloned() {
            store.order.remove(0);
            store.paths.remove(&old);
        } else {
            break;
        }
    }
    if let Err(e) = save_store(&store) {
        crate::ops_log::log("ERR", &format!("last_cwd save failed: {e}"));
    } else {
        crate::ops_log::log(
            "CWD",
            &format!(
                "last_cwd saved key={} path={}",
                endpoint_key(host, username),
                path
            ),
        );
    }
}

/// Drop a poisoned / missing path so the next connect does not re-try it.
pub fn clear_last_cwd(host: &str, username: &str) {
    if host.trim().is_empty() || username.trim().is_empty() {
        return;
    }
    let key = endpoint_key(host, username);
    let mut store = match load_store() {
        Ok(s) => s,
        Err(_) => return,
    };
    if store.paths.remove(&key).is_none() {
        return;
    }
    store.order.retain(|k| k != &key);
    let _ = save_store(&store);
    crate::ops_log::log("CWD", &format!("last_cwd cleared key={key}"));
}

#[cfg(test)]
mod tests {
    use super::endpoint_key;

    #[test]
    fn endpoint_key_normalizes_host() {
        assert_eq!(
            endpoint_key("Host.Example", "alice"),
            "alice@host.example"
        );
        assert_eq!(endpoint_key("  1.2.3.4 ", " bob "), "bob@1.2.3.4");
    }
}
