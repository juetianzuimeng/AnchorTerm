pub mod last_cwd;
pub mod profile;

use std::fs;
use std::path::PathBuf;

use crate::config::profile::{HostProfile, ProfileStore};
use crate::error::AppError;

pub use last_cwd::{clear_last_cwd, load_last_cwd, save_last_cwd};

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

pub fn load_profiles() -> Result<ProfileStore, AppError> {
    let path = profiles_path()?;
    if !path.exists() {
        return Ok(ProfileStore::default());
    }
    let raw = fs::read_to_string(&path)?;
    let store: ProfileStore = serde_json::from_str(&raw)?;
    Ok(store)
}

pub fn save_profiles(store: &ProfileStore) -> Result<(), AppError> {
    let path = profiles_path()?;
    let raw = serde_json::to_string_pretty(store)?;
    fs::write(path, raw)?;
    Ok(())
}

pub fn list_profiles() -> Result<Vec<HostProfile>, AppError> {
    Ok(load_profiles()?.profiles)
}

pub fn upsert_profile(profile: HostProfile) -> Result<HostProfile, AppError> {
    let mut store = load_profiles()?;
    if let Some(existing) = store.profiles.iter_mut().find(|p| p.id == profile.id) {
        *existing = profile.clone();
    } else {
        store.profiles.push(profile.clone());
    }
    save_profiles(&store)?;
    Ok(profile)
}

pub fn delete_profile(id: &str) -> Result<(), AppError> {
    let mut store = load_profiles()?;
    store.profiles.retain(|p| p.id != id);
    save_profiles(&store)?;
    crate::auth::credentials::delete_password(id)?;
    Ok(())
}

pub fn get_profile(id: &str) -> Result<Option<HostProfile>, AppError> {
    Ok(load_profiles()?.profiles.into_iter().find(|p| p.id == id))
}
