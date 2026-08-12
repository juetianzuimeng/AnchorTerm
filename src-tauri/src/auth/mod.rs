pub mod credentials;

use std::fmt;

use serde::{Deserialize, Serialize};

/// Authentication method provided by the UI for a connect attempt.
/// Debug is redacted — never log secrets.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthMethod {
    Password {
        /// Optional when loading from keyring via profile_id.
        password: Option<String>,
        /// Persist password to OS credential store under profile_id.
        #[serde(default)]
        save_password: bool,
    },
    PublicKey {
        private_key_path: String,
        passphrase: Option<String>,
        /// Persist passphrase to OS credential store under profile_id.
        #[serde(default)]
        save_passphrase: bool,
    },
}

impl fmt::Debug for AuthMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthMethod::Password {
                password,
                save_password,
            } => f
                .debug_struct("Password")
                .field(
                    "password",
                    &password.as_ref().map(|_| "[redacted]").unwrap_or("[none]"),
                )
                .field("save_password", save_password)
                .finish(),
            AuthMethod::PublicKey {
                private_key_path,
                passphrase,
                save_passphrase,
            } => f
                .debug_struct("PublicKey")
                .field("private_key_path", private_key_path)
                .field(
                    "passphrase",
                    &passphrase.as_ref().map(|_| "[redacted]").unwrap_or("[none]"),
                )
                .field("save_passphrase", save_passphrase)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    Password,
    PublicKey,
}

impl From<&AuthMethod> for AuthType {
    fn from(value: &AuthMethod) -> Self {
        match value {
            AuthMethod::Password { .. } => AuthType::Password,
            AuthMethod::PublicKey { .. } => AuthType::PublicKey,
        }
    }
}
