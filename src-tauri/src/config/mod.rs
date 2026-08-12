pub mod last_cwd;
pub mod profile;
pub mod transfer;

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::config::profile::{HostProfile, ProfileStore};
use crate::error::AppError;

pub use last_cwd::{clear_last_cwd, load_last_cwd, save_last_cwd};
pub use transfer::{
    export_profiles, import_profiles, ExportRequest, ExportResult, ImportRequest, ImportResult,
};

/// Serialize all profiles.json + related flag mutations (import/list/upsert/delete).
static PROFILES_LOCK: Mutex<()> = Mutex::new(());

pub fn with_profiles_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = PROFILES_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    f()
}

pub fn config_dir() -> Result<PathBuf, AppError> {
    let base = dirs::config_dir().ok_or_else(|| {
        AppError::Config("无法定位用户配置目录".into())
    })?;
    let dir = base.join("AnchorTerm");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn profiles_path() -> Result<PathBuf, AppError> {
    Ok(config_dir()?.join("profiles.json"))
}

/// Unlocked load — callers that mutate must hold [`with_profiles_lock`].
pub fn load_profiles_unlocked() -> Result<ProfileStore, AppError> {
    let path = profiles_path()?;
    if !path.exists() {
        return Ok(ProfileStore::default());
    }
    let raw = fs::read_to_string(&path)?;
    let store: ProfileStore = serde_json::from_str(&raw)?;
    Ok(store)
}

pub fn save_profiles_unlocked(store: &ProfileStore) -> Result<(), AppError> {
    let path = profiles_path()?;
    let raw = serde_json::to_string_pretty(store)?;
    fs::write(path, raw)?;
    Ok(())
}

#[allow(dead_code)]
pub fn load_profiles() -> Result<ProfileStore, AppError> {
    with_profiles_lock(load_profiles_unlocked)
}

#[allow(dead_code)]
pub fn save_profiles(store: &ProfileStore) -> Result<(), AppError> {
    with_profiles_lock(|| save_profiles_unlocked(store))
}

/// Soft-reconcile keyring flags for UI only — does **not** rewrite profiles.json
/// (avoids list-time RMW races and keyring thrash).
pub fn list_profiles() -> Result<Vec<HostProfile>, AppError> {
    with_profiles_lock(|| {
        let store = load_profiles_unlocked()?;
        let mut out = store.profiles;
        for p in &mut out {
            if p.has_saved_password {
                let ok = crate::auth::credentials::load_password(&p.id)
                    .ok()
                    .flatten()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if !ok {
                    p.has_saved_password = false;
                }
            }
            if p.has_saved_passphrase {
                let ok = crate::auth::credentials::load_passphrase(&p.id)
                    .ok()
                    .flatten()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if !ok {
                    p.has_saved_passphrase = false;
                }
            }
        }
        Ok(out)
    })
}

pub fn upsert_profile(profile: HostProfile) -> Result<HostProfile, AppError> {
    with_profiles_lock(|| {
        let mut store = load_profiles_unlocked()?;
        if let Some(existing) = store.profiles.iter_mut().find(|p| p.id == profile.id) {
            *existing = profile.clone();
        } else {
            store.profiles.push(profile.clone());
        }
        save_profiles_unlocked(&store)?;
        Ok(profile)
    })
}

pub fn delete_profile(id: &str) -> Result<(), AppError> {
    with_profiles_lock(|| {
        let mut store = load_profiles_unlocked()?;
        store.profiles.retain(|p| p.id != id);
        save_profiles_unlocked(&store)?;
        crate::auth::credentials::delete_all_secrets(id)?;
        Ok(())
    })
}

pub fn get_profile(id: &str) -> Result<Option<HostProfile>, AppError> {
    with_profiles_lock(|| {
        Ok(load_profiles_unlocked()?
            .profiles
            .into_iter()
            .find(|p| p.id == id))
    })
}
