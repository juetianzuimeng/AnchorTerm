//! Persist SSH port-forwarding rules per host, so they survive tab close /
//! app restart and can be auto-restored on the next connect.
//!
//! Disk layout (`%APPDATA%/AnchorTerm/forwards.json`):
//!
//! ```json
//! {
//!   "endpoints": {
//!     "alice@example:22": {
//!       "auto_restore": true,
//!       "rules": [ { "id": "...", "kind": "local", "bind_address": "127.0.0.1",
//!                    "listen_port": 6379, "dest_host": "127.0.0.1", "dest_port": 6379 } ]
//!     }
//!   },
//!   "order": ["alice@example:22"]
//! }
//! ```
//!
//! Key = `user@host:ssh_port` (host lowercased). Disk is the single source of
//! truth per endpoint; writes are id-scoped (upsert / remove) so concurrent
//! tabs to the same host never clobber each other's rules.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::config_dir;
use crate::error::AppError;
use crate::ssh::forward::ForwardSpec;

const MAX_ENDPOINTS: usize = 50;
const MAX_RULES_PER_ENDPOINT: usize = 8;

static FORWARDS_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn with_forwards_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = FORWARDS_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    f()
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct EndpointForwards {
    /// Auto-restore this endpoint's rules when a new session connects.
    #[serde(default)]
    pub auto_restore: bool,
    /// Saved rules (stable `id` each).
    #[serde(default)]
    pub rules: Vec<ForwardSpec>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ForwardsStore {
    #[serde(default)]
    endpoints: HashMap<String, EndpointForwards>,
    /// Insertion/update order for LRU eviction (newest last).
    #[serde(default)]
    order: Vec<String>,
}

/// `user@host:port` (host lowercased) — the per-endpoint persistence key.
pub fn endpoint_key(host: &str, port: u16, username: &str) -> String {
    format!(
        "{}@{}:{}",
        username.trim(),
        host.trim().to_ascii_lowercase(),
        port
    )
}

fn store_path() -> Result<PathBuf, AppError> {
    Ok(config_dir()?.join("forwards.json"))
}

fn load_store() -> Result<ForwardsStore, AppError> {
    let path = store_path()?;
    if !path.exists() {
        return Ok(ForwardsStore::default());
    }
    let raw = fs::read_to_string(&path)?;
    if raw.trim().is_empty() {
        return Ok(ForwardsStore::default());
    }
    // Fast path: the whole file parses cleanly.
    if let Ok(store) = serde_json::from_str::<ForwardsStore>(&raw) {
        return Ok(store);
    }
    // Best-effort: a single malformed rule/endpoint must not wipe the rest.
    rebuild_store_best_effort(&raw)
}

/// Recover as many valid rules as possible when the whole-file parse fails.
/// Invalid rules (missing `id`/port, or unparseable) are skipped with a log.
fn rebuild_store_best_effort(raw: &str) -> Result<ForwardsStore, AppError> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
        AppError::Config(format!("forwards.json 不是合法 JSON: {e}"))
    })?;
    let mut store = ForwardsStore::default();
    let endpoints = v
        .get("endpoints")
        .and_then(|e| e.as_object())
        .cloned()
        .unwrap_or_default();
    for (key, ep_val) in endpoints {
        let auto_restore = ep_val
            .get("auto_restore")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let mut rules: Vec<ForwardSpec> = Vec::new();
        if let Some(arr) = ep_val.get("rules").and_then(|r| r.as_array()) {
            for r in arr {
                match serde_json::from_value::<ForwardSpec>(r.clone()) {
                    Ok(spec)
                        if !spec.id.is_empty()
                            && spec.listen_port != 0
                            && spec.dest_port != 0 =>
                    {
                        rules.push(spec);
                    }
                    Ok(_) => crate::ops_log::log(
                        "FWD",
                        &format!("forwards 跳过无效规则（缺失 id/端口）key={key}"),
                    ),
                    Err(e) => crate::ops_log::log(
                        "FWD",
                        &format!("forwards 跳过无法解析的规则 key={key}: {e}"),
                    ),
                }
            }
        }
        if !rules.is_empty() || auto_restore {
            store.endpoints.insert(
                key.clone(),
                EndpointForwards {
                    auto_restore,
                    rules,
                },
            );
            store.order.push(key);
        }
    }
    crate::ops_log::log(
        "FWD",
        "forwards.json 已按规则容错加载（跳过损坏条目）",
    );
    Ok(store)
}

fn save_store(store: &ForwardsStore) -> Result<(), AppError> {
    let path = store_path()?;
    let raw = serde_json::to_string_pretty(store)?;
    fs::write(path, raw)?;
    Ok(())
}

/// Merge `spec` into `rules` by stable `id`. Updating an existing rule moves it
/// to the end (most-recently-used) so a frequently-used rule isn't evicted by a
/// one-shot. Caps at [`MAX_RULES_PER_ENDPOINT`] by evicting the oldest entry.
fn merge_rule(rules: &mut Vec<ForwardSpec>, spec: ForwardSpec) {
    if let Some(pos) = rules.iter().position(|r| r.id == spec.id) {
        rules.remove(pos);
        rules.push(spec);
        return;
    }
    if rules.len() >= MAX_RULES_PER_ENDPOINT {
        rules.remove(0);
    }
    rules.push(spec);
}

/// Move `key` to the end of `order`; if over [`MAX_ENDPOINTS`], evict the
/// oldest and return its key so the caller can drop it from `endpoints`.
fn touch_order(order: &mut Vec<String>, key: &str) -> Option<String> {
    order.retain(|k| k != key);
    order.push(key.to_string());
    if order.len() > MAX_ENDPOINTS {
        Some(order.remove(0))
    } else {
        None
    }
}

/// Load the persisted entry for an endpoint (rules + auto_restore flag).
pub fn load_persisted(host: &str, port: u16, username: &str) -> Option<EndpointForwards> {
    let key = endpoint_key(host, port, username);
    let store = load_store().ok()?;
    store.endpoints.get(&key).cloned()
}

/// Persist (upsert) a single rule by its stable `id`.
pub fn upsert_rule(host: &str, port: u16, username: &str, spec: &ForwardSpec) {
    if host.trim().is_empty() || username.trim().is_empty() {
        return;
    }
    let key = endpoint_key(host, port, username);
    with_forwards_lock(|| {
        let mut store = match load_store() {
            Ok(s) => s,
            Err(e) => {
                crate::ops_log::log("ERR", &format!("forwards load failed: {e}"));
                return;
            }
        };
        let ep = store.endpoints.entry(key.clone()).or_default();
        merge_rule(&mut ep.rules, spec.clone());
        if let Some(evicted) = touch_order(&mut store.order, &key) {
            store.endpoints.remove(&evicted);
        }
        if let Err(e) = save_store(&store) {
            crate::ops_log::log("ERR", &format!("forwards save failed: {e}"));
        } else {
            crate::ops_log::log(
                "FWD",
                &format!("forwards upsert key={} id={}", key, &spec.id[..spec.id.len().min(8)]),
            );
        }
    });
}

/// Remove a single rule by `id`.
pub fn remove_rule(host: &str, port: u16, username: &str, id: &str) {
    if host.trim().is_empty() || username.trim().is_empty() || id.trim().is_empty() {
        return;
    }
    let key = endpoint_key(host, port, username);
    with_forwards_lock(|| {
        let mut store = match load_store() {
            Ok(s) => s,
            Err(e) => {
                crate::ops_log::log("ERR", &format!("forwards load failed: {e}"));
                return;
            }
        };
        if let Some(ep) = store.endpoints.get_mut(&key) {
            if ep.rules.iter().any(|r| r.id == id) {
                ep.rules.retain(|r| r.id != id);
                if let Err(e) = save_store(&store) {
                    crate::ops_log::log("ERR", &format!("forwards save failed: {e}"));
                } else {
                    crate::ops_log::log("FWD", &format!("forwards remove key={} id={}", key, &id[..id.len().min(8)]));
                }
            }
        }
    });
}

/// Set the per-endpoint auto-restore switch.
pub fn set_auto_restore(host: &str, port: u16, username: &str, on: bool) {
    if host.trim().is_empty() || username.trim().is_empty() {
        return;
    }
    let key = endpoint_key(host, port, username);
    with_forwards_lock(|| {
        let mut store = match load_store() {
            Ok(s) => s,
            Err(e) => {
                crate::ops_log::log("ERR", &format!("forwards load failed: {e}"));
                return;
            }
        };
        store.endpoints.entry(key.clone()).or_default().auto_restore = on;
        if let Some(evicted) = touch_order(&mut store.order, &key) {
            store.endpoints.remove(&evicted);
        }
        if let Err(e) = save_store(&store) {
            crate::ops_log::log("ERR", &format!("forwards save failed: {e}"));
        } else {
            crate::ops_log::log("FWD", &format!("forwards auto_restore key={} on={on}", key));
        }
    });
}

/// Read the per-endpoint auto-restore switch (defaults to false).
pub fn get_auto_restore(host: &str, port: u16, username: &str) -> bool {
    load_persisted(host, port, username)
        .map(|ep| ep.auto_restore)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_key_normalizes_host_and_includes_port() {
        assert_eq!(
            endpoint_key("Host.Example", 2222, "Alice"),
            "Alice@host.example:2222"
        );
        assert_eq!(
            endpoint_key("  1.2.3.4 ", 22, " bob "),
            "bob@1.2.3.4:22"
        );
    }

    #[test]
    fn merge_rule_upserts_by_id_without_clobbering_others() {
        let mut rules: Vec<ForwardSpec> = vec![ForwardSpec {
            id: "a".into(),
            kind: crate::ssh::forward::ForwardKind::Local,
            bind_address: "127.0.0.1".into(),
            listen_port: 6379,
            dest_host: "10.0.0.1".into(),
            dest_port: 6379,
        }];
        // Update existing id "a".
        merge_rule(
            &mut rules,
            ForwardSpec {
                id: "a".into(),
                kind: crate::ssh::forward::ForwardKind::Local,
                bind_address: "127.0.0.1".into(),
                listen_port: 6379,
                dest_host: "10.0.0.2".into(),
                dest_port: 6379,
            },
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].dest_host, "10.0.0.2");
        // Append new id "b" — does not overwrite "a".
        merge_rule(
            &mut rules,
            ForwardSpec {
                id: "b".into(),
                kind: crate::ssh::forward::ForwardKind::Local,
                bind_address: "127.0.0.1".into(),
                listen_port: 6380,
                dest_host: "10.0.0.3".into(),
                dest_port: 6380,
            },
        );
        assert_eq!(rules.len(), 2);
    }

    #[test]
    fn serde_roundtrip_endpoint_forwards() {
        let spec = ForwardSpec {
            id: "x".into(),
            kind: crate::ssh::forward::ForwardKind::Remote,
            bind_address: "127.0.0.1".into(),
            listen_port: 8080,
            dest_host: "127.0.0.1".into(),
            dest_port: 3000,
        };
        let ep = EndpointForwards {
            auto_restore: true,
            rules: vec![spec],
        };
        let json = serde_json::to_string(&ep).unwrap();
        let back: EndpointForwards = serde_json::from_str(&json).unwrap();
        assert!(back.auto_restore);
        assert_eq!(back.rules.len(), 1);
        assert_eq!(back.rules[0].kind, crate::ssh::forward::ForwardKind::Remote);
    }

    #[test]
    fn rebuild_store_skips_malformed_rule() {
        let raw = r#"{
            "endpoints": {
                "alice@host:22": {
                    "auto_restore": true,
                    "rules": [
                        {"id":"good","kind":"local","bind_address":"127.0.0.1","listen_port":6379,"dest_host":"10.0.0.1","dest_port":6379},
                        {"id":"","kind":"local","listen_port":0,"dest_host":"x","dest_port":0},
                        {"nope": true}
                    ]
                }
            },
            "order": ["alice@host:22"]
        }"#;
        let store = rebuild_store_best_effort(raw).unwrap();
        let ep = store.endpoints.get("alice@host:22").unwrap();
        assert!(ep.auto_restore);
        assert_eq!(ep.rules.len(), 1, "malformed rules must be skipped");
        assert_eq!(ep.rules[0].id, "good");
    }
}
