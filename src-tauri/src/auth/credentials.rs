use crate::error::AppError;

const SERVICE: &str = "AnchorTerm";

/// Login password entry (account password auth).
fn entry_password(profile_id: &str) -> Result<keyring::Entry, AppError> {
    keyring::Entry::new(SERVICE, profile_id).map_err(AppError::from)
}

/// Private-key passphrase entry (public_key auth). Separate user slot so it
/// never collides with login password for the same profile id.
fn entry_passphrase(profile_id: &str) -> Result<keyring::Entry, AppError> {
    keyring::Entry::new(SERVICE, &format!("{profile_id}:passphrase")).map_err(AppError::from)
}

fn set_secret(entry: keyring::Entry, secret: &str) -> Result<(), AppError> {
    entry.set_password(secret)?;
    Ok(())
}

fn load_secret(entry: keyring::Entry) -> Result<Option<String>, AppError> {
    match entry.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(AppError::from(e)),
    }
}

fn delete_secret(entry: keyring::Entry) -> Result<(), AppError> {
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(AppError::from(e)),
    }
}

/// Store login password in OS credential store (Windows Credential Manager).
pub fn save_password(profile_id: &str, password: &str) -> Result<(), AppError> {
    set_secret(entry_password(profile_id)?, password)
}

pub fn load_password(profile_id: &str) -> Result<Option<String>, AppError> {
    load_secret(entry_password(profile_id)?)
}

pub fn delete_password(profile_id: &str) -> Result<(), AppError> {
    delete_secret(entry_password(profile_id)?)
}

/// Store private-key passphrase in OS credential store (never in profiles.json).
pub fn save_passphrase(profile_id: &str, passphrase: &str) -> Result<(), AppError> {
    set_secret(entry_passphrase(profile_id)?, passphrase)
}

pub fn load_passphrase(profile_id: &str) -> Result<Option<String>, AppError> {
    load_secret(entry_passphrase(profile_id)?)
}

pub fn delete_passphrase(profile_id: &str) -> Result<(), AppError> {
    delete_secret(entry_passphrase(profile_id)?)
}

/// Drop all secrets for a profile (login password + key passphrase).
pub fn delete_all_secrets(profile_id: &str) -> Result<(), AppError> {
    delete_password(profile_id)?;
    delete_passphrase(profile_id)?;
    Ok(())
}
