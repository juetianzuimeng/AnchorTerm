use crate::error::AppError;

const SERVICE: &str = "AnchorTerm";

fn entry(profile_id: &str) -> Result<keyring::Entry, AppError> {
    keyring::Entry::new(SERVICE, profile_id).map_err(AppError::from)
}

/// Store password in OS credential store (Windows Credential Manager / DPAPI backend).
pub fn save_password(profile_id: &str, password: &str) -> Result<(), AppError> {
    entry(profile_id)?.set_password(password)?;
    Ok(())
}

pub fn load_password(profile_id: &str) -> Result<Option<String>, AppError> {
    match entry(profile_id)?.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(AppError::from(e)),
    }
}

pub fn delete_password(profile_id: &str) -> Result<(), AppError> {
    match entry(profile_id)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(AppError::from(e)),
    }
}
