//! Session profile import / export for machine migration.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::credentials;
use crate::auth::AuthType;
use crate::config::profile::{HostProfile, ProfileStore};
use crate::config::{
    load_profiles_unlocked, save_profiles_unlocked, with_profiles_lock,
};
use crate::error::AppError;
use crate::ops_log;

pub const EXPORT_FORMAT: &str = "anchorterm.profiles";
pub const EXPORT_VERSION: u32 = 1;

/// One profile row in an export file (may carry secrets for seamless migrate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferProfile {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_type: AuthType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    #[serde(default = "default_true")]
    pub reconnect_enabled: bool,
    /// Login password (only when include_secrets on export).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Private-key passphrase (only when include_secrets on export).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesTransferFile {
    #[serde(default)]
    pub format: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub exported_at: Option<String>,
    #[serde(default)]
    pub include_secrets: bool,
    pub profiles: Vec<TransferProfile>,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    /// Pretty JSON (also written to `saved_path` when user picks a file).
    pub json: String,
    pub profile_count: usize,
    pub secrets_count: usize,
    pub include_secrets: bool,
    /// Absolute path after native save dialog; None if user cancelled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_path: Option<String>,
    /// True when the user dismissed the save dialog.
    pub cancelled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    /// When true, pull passwords/passphrases from the OS keyring into the file.
    #[serde(default)]
    pub include_secrets: bool,
    /// If non-empty, only export these profile ids; empty/omitted = all.
    #[serde(default)]
    pub profile_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    /// Upsert by id; new ids are created when missing or when generate_new_ids.
    Merge,
    /// Replace entire local list with imported profiles.
    Replace,
}

impl Default for ImportMode {
    fn default() -> Self {
        ImportMode::Merge
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRequest {
    /// Full transfer JSON text.
    pub json: String,
    #[serde(default)]
    pub mode: ImportMode,
    /// Force new UUIDs (treat as copies; good when importing onto same machine).
    #[serde(default)]
    pub generate_new_ids: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub imported: usize,
    pub updated: usize,
    pub created: usize,
    pub secrets_restored: usize,
    pub skipped: usize,
    pub mode: String,
    pub warnings: Vec<String>,
}

pub fn export_profiles(req: ExportRequest) -> Result<ExportResult, AppError> {
    with_profiles_lock(|| {
        let store = load_profiles_unlocked()?;
        let filter: Option<std::collections::HashSet<&str>> = req
            .profile_ids
            .as_ref()
            .filter(|ids| !ids.is_empty())
            .map(|ids| ids.iter().map(|s| s.as_str()).collect());

        let selected: Vec<&HostProfile> = store
            .profiles
            .iter()
            .filter(|p| match &filter {
                Some(set) => set.contains(p.id.as_str()),
                None => true,
            })
            .collect();

        if selected.is_empty() {
            return Err(AppError::Config(
                "没有可导出的会话（请先选中一条，或确认本地已有配置）".into(),
            ));
        }

        let mut secrets_count = 0usize;
        let mut out = Vec::with_capacity(selected.len());

        for p in &selected {
            let mut tp = TransferProfile {
                id: Some(p.id.clone()),
                name: p.name.clone(),
                host: p.host.clone(),
                port: p.port,
                username: p.username.clone(),
                auth_type: p.auth_type,
                private_key_path: p.private_key_path.clone(),
                reconnect_enabled: p.reconnect_enabled,
                password: None,
                passphrase: None,
            };

            if req.include_secrets {
                match p.auth_type {
                    AuthType::Password => {
                        if let Ok(Some(pw)) = credentials::load_password(&p.id) {
                            if !pw.is_empty() {
                                tp.password = Some(pw);
                                secrets_count += 1;
                            }
                        }
                    }
                    AuthType::PublicKey => {
                        if let Ok(Some(pp)) = credentials::load_passphrase(&p.id) {
                            if !pp.is_empty() {
                                tp.passphrase = Some(pp);
                                secrets_count += 1;
                            }
                        }
                    }
                }
            }
            out.push(tp);
        }

        let file = ProfilesTransferFile {
            format: EXPORT_FORMAT.into(),
            version: EXPORT_VERSION,
            exported_at: Some(format!(
                "unix:{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            )),
            include_secrets: req.include_secrets,
            profiles: out,
        };

        let json = serde_json::to_string_pretty(&file)?;
        ops_log::log(
            "CFG",
            &format!(
                "export_profiles count={} secrets={} include_secrets={} filtered={}",
                file.profiles.len(),
                secrets_count,
                req.include_secrets,
                filter.is_some()
            ),
        );

        // Native "Save As" — default name under Downloads (or Desktop).
        let default_name = {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if file.profiles.len() == 1 {
                let safe: String = file.profiles[0]
                    .name
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .take(40)
                    .collect();
                format!("AnchorTerm-profile-{safe}-{secs}.json")
            } else {
                format!("AnchorTerm-profiles-{secs}.json")
            }
        };
        let start_dir = dirs::download_dir()
            .or_else(dirs::desktop_dir)
            .or_else(dirs::home_dir);

        let picked = {
            let mut dlg = rfd::FileDialog::new();
            dlg = dlg
                .set_title("导出会话配置")
                .set_file_name(&default_name)
                .add_filter("JSON", &["json"]);
            if let Some(dir) = start_dir {
                dlg = dlg.set_directory(dir);
            }
            dlg.save_file()
        };

        let Some(path) = picked else {
            ops_log::log("CFG", "export_profiles cancelled by user");
            return Ok(ExportResult {
                profile_count: file.profiles.len(),
                secrets_count,
                include_secrets: req.include_secrets,
                json,
                saved_path: None,
                cancelled: true,
            });
        };

        std::fs::write(&path, json.as_bytes()).map_err(|e| {
            AppError::Io(format!("写入导出文件失败 {}: {e}", path.display()))
        })?;

        ops_log::log(
            "CFG",
            &format!("export_profiles saved path={}", path.display()),
        );

        Ok(ExportResult {
            profile_count: file.profiles.len(),
            secrets_count,
            include_secrets: req.include_secrets,
            json,
            saved_path: Some(path.display().to_string()),
            cancelled: false,
        })
    })
}

fn store_to_transfer(store: ProfileStore) -> ProfilesTransferFile {
    ProfilesTransferFile {
        format: EXPORT_FORMAT.into(),
        version: 1,
        exported_at: None,
        include_secrets: false,
        profiles: store
            .profiles
            .into_iter()
            .map(|p| TransferProfile {
                id: Some(p.id),
                name: p.name,
                host: p.host,
                port: p.port,
                username: p.username,
                auth_type: p.auth_type,
                private_key_path: p.private_key_path,
                reconnect_enabled: p.reconnect_enabled,
                password: None,
                passphrase: None,
            })
            .collect(),
    }
}

pub fn import_profiles(req: ImportRequest) -> Result<ImportResult, AppError> {
    // 1) Official transfer envelope
    if let Ok(file) = serde_json::from_str::<ProfilesTransferFile>(&req.json) {
        if file.format == EXPORT_FORMAT
            || file.format == "anchorterm-profiles"
            || file.format.is_empty()
        {
            if file.version > EXPORT_VERSION {
                return Err(AppError::Config(format!(
                    "导出文件版本过高 ({} > {})，请升级 AnchorTerm",
                    file.version, EXPORT_VERSION
                )));
            }
            return import_from_transfer(file, req.mode, req.generate_new_ids);
        }
        return Err(AppError::Config(format!(
            "不支持的导出格式: {}（期望 {}）",
            file.format, EXPORT_FORMAT
        )));
    }

    // 2) Raw profiles.json { "profiles": [ ... ] }
    if let Ok(store) = serde_json::from_str::<ProfileStore>(&req.json) {
        return import_from_transfer(store_to_transfer(store), req.mode, req.generate_new_ids);
    }

    // 3) Bare array of profiles
    if let Ok(list) = serde_json::from_str::<Vec<HostProfile>>(&req.json) {
        return import_from_transfer(
            store_to_transfer(ProfileStore { profiles: list }),
            req.mode,
            req.generate_new_ids,
        );
    }

    Err(AppError::Config(
        "无法解析导入文件（需要 AnchorTerm 导出 JSON 或 profiles.json）".into(),
    ))
}

fn import_from_transfer(
    file: ProfilesTransferFile,
    mode: ImportMode,
    generate_new_ids: bool,
) -> Result<ImportResult, AppError> {
    if file.profiles.is_empty() {
        return Err(AppError::Config("导入文件中没有会话配置".into()));
    }

    with_profiles_lock(|| {
        let mut warnings = Vec::new();
        let mut created = 0usize;
        let mut updated = 0usize;
        let mut secrets_restored = 0usize;
        let mut skipped = 0usize;

        // Snapshot old ids; never delete keyring until profiles.json is committed.
        let old_store = load_profiles_unlocked()?;
        let old_ids: Vec<String> = old_store.profiles.iter().map(|p| p.id.clone()).collect();

        let mut store = match mode {
            ImportMode::Replace => ProfileStore::default(),
            ImportMode::Merge => old_store.clone(),
        };

        // Pending secret writes (applied after rows validated into `store`).
        let mut pending_secrets: Vec<(String, AuthType, Option<String>, Option<String>)> =
            Vec::new();

        for tp in file.profiles {
            let name = tp.name.trim();
            let host = tp.host.trim();
            let username = tp.username.trim();
            if name.is_empty() || host.is_empty() || username.is_empty() {
                skipped += 1;
                warnings.push("跳过一条无效配置（名称/主机/用户名为空）".into());
                continue;
            }
            if tp.port == 0 {
                skipped += 1;
                warnings.push(format!("跳过 {name}：端口无效"));
                continue;
            }
            if tp.auth_type == AuthType::PublicKey {
                let path = tp.private_key_path.as_deref().unwrap_or("").trim();
                if path.is_empty() {
                    warnings.push(format!(
                        "配置「{name}」为私钥认证但无私钥路径，已导入；迁移后请在目标机修正路径"
                    ));
                } else {
                    warnings.push(format!(
                        "配置「{name}」含私钥路径，目标机路径可能不同，请确认文件仍存在"
                    ));
                }
            }

            let incoming_id = tp
                .id
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            let (id, is_update) = if generate_new_ids {
                (Uuid::new_v4().to_string(), false)
            } else if let Some(ref iid) = incoming_id {
                if store.profiles.iter().any(|p| p.id == *iid) {
                    (iid.clone(), true)
                } else {
                    (iid.clone(), false)
                }
            } else {
                (Uuid::new_v4().to_string(), false)
            };

            let mut has_saved_password = false;
            let mut has_saved_passphrase = false;
            if is_update {
                if let Some(old) = store.profiles.iter().find(|p| p.id == id) {
                    has_saved_password = old.has_saved_password;
                    has_saved_passphrase = old.has_saved_passphrase;
                }
            }

            let password = tp
                .password
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let passphrase = tp
                .passphrase
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            match tp.auth_type {
                AuthType::Password => {
                    if password.is_some() {
                        has_saved_password = true;
                        has_saved_passphrase = false;
                    } else if !is_update {
                        has_saved_password = false;
                    } else {
                        has_saved_password = credentials::load_password(&id)
                            .ok()
                            .flatten()
                            .map(|s| !s.is_empty())
                            .unwrap_or(false);
                    }
                }
                AuthType::PublicKey => {
                    if passphrase.is_some() {
                        has_saved_passphrase = true;
                        has_saved_password = false;
                    } else if !is_update {
                        has_saved_passphrase = false;
                    } else {
                        has_saved_passphrase = credentials::load_passphrase(&id)
                            .ok()
                            .flatten()
                            .map(|s| !s.is_empty())
                            .unwrap_or(false);
                    }
                }
            }

            let profile = HostProfile {
                id: id.clone(),
                name: name.to_string(),
                host: host.to_string(),
                port: tp.port,
                username: username.to_string(),
                auth_type: tp.auth_type,
                private_key_path: tp
                    .private_key_path
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                has_saved_password,
                has_saved_passphrase,
                reconnect_enabled: tp.reconnect_enabled,
            };

            if is_update {
                if let Some(slot) = store.profiles.iter_mut().find(|p| p.id == id) {
                    *slot = profile;
                }
                updated += 1;
            } else {
                store.profiles.push(profile);
                created += 1;
            }

            pending_secrets.push((id, tp.auth_type, password, passphrase));
        }

        // Commit secrets for the final set, then profiles.json, then prune orphans.
        for (id, auth_type, password, passphrase) in &pending_secrets {
            match auth_type {
                AuthType::Password => {
                    let _ = credentials::delete_passphrase(id);
                    if let Some(pw) = password {
                        credentials::save_password(id, pw)?;
                        secrets_restored += 1;
                    }
                }
                AuthType::PublicKey => {
                    let _ = credentials::delete_password(id);
                    if let Some(pp) = passphrase {
                        credentials::save_passphrase(id, pp)?;
                        secrets_restored += 1;
                    }
                }
            }
        }

        save_profiles_unlocked(&store)?;

        // Only after durable profiles write: drop secrets for removed ids (replace/merge drops).
        let new_ids: std::collections::HashSet<&str> =
            store.profiles.iter().map(|p| p.id.as_str()).collect();
        for oid in &old_ids {
            if !new_ids.contains(oid.as_str()) {
                let _ = credentials::delete_all_secrets(oid);
            }
        }

        ops_log::log(
            "CFG",
            &format!(
                "import_profiles mode={mode:?} created={created} updated={updated} secrets={secrets_restored} skipped={skipped}"
            ),
        );

        Ok(ImportResult {
            imported: created + updated,
            updated,
            created,
            secrets_restored,
            skipped,
            mode: match mode {
                ImportMode::Merge => "merge".into(),
                ImportMode::Replace => "replace".into(),
            },
            warnings,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip_shape() {
        let raw = r#"{
          "format": "anchorterm.profiles",
          "version": 1,
          "include_secrets": false,
          "profiles": [
            {
              "id": "abc",
              "name": "lab",
              "host": "1.2.3.4",
              "port": 22,
              "username": "root",
              "auth_type": "password"
            }
          ]
        }"#;
        let f: ProfilesTransferFile = serde_json::from_str(raw).unwrap();
        assert_eq!(f.profiles.len(), 1);
        assert_eq!(f.profiles[0].host, "1.2.3.4");
    }
}
