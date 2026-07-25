use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::AuthType;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_type: AuthType,
    /// Absolute path to OpenSSH private key (public_key auth only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    /// Whether a password is stored in the OS keyring for this profile.
    #[serde(default)]
    pub has_saved_password: bool,
    #[serde(default)]
    pub reconnect_enabled: bool,
}

impl HostProfile {
    pub fn new(
        name: String,
        host: String,
        port: u16,
        username: String,
        auth_type: AuthType,
        private_key_path: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            host,
            port,
            username,
            auth_type,
            private_key_path,
            has_saved_password: false,
            reconnect_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileStore {
    pub profiles: Vec<HostProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveProfileRequest {
    /// If set, update existing; otherwise create.
    pub id: Option<String>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_type: AuthType,
    pub private_key_path: Option<String>,
    /// When true and password provided, store in keyring.
    #[serde(default)]
    pub save_password: bool,
    /// Password to save (never persisted in profiles.json).
    pub password: Option<String>,
}
