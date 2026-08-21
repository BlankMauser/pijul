use super::Complete;
use super::fix_identities;

use crate::IdentityError;
use pijul_core::key::PublicKey;

use std::fs;
use std::path::PathBuf;

use std::sync::OnceLock;

static CHOSEN_IDENTITY: OnceLock<String> = OnceLock::new();

/// Returns the directory in which identity information should be stored.
pub fn path(name: &str, should_exist: bool) -> Result<PathBuf, IdentityError> {
    if name.is_empty() {
        return Err(IdentityError::NotFound(PathBuf::new()));
    }

    let config_directory = pijul_config::global_config_directory()?;
    let path = config_directory.join("identities").join(name);

    if !path.exists() && should_exist {
        return Err(IdentityError::NotFound(path));
    }

    Ok(path)
}

/// Returns the public key for identity named `name`.
pub fn public_key(name: &str) -> Result<PublicKey, IdentityError> {
    let text = fs::read_to_string(path(name, true)?.join("identity.toml"))?;
    let identity: Complete = toml::from_str(&text)?;
    Ok(identity.public_key)
}

/// Choose an identity, either through defaults or a user prompt.
pub async fn choose_identity_name(config: &pijul_config::Config) -> Result<String, IdentityError> {
    if let Some(name) = CHOSEN_IDENTITY.get() {
        return Ok(name.clone());
    }

    let mut possible_identities = Complete::load_all()?;
    if possible_identities.is_empty() {
        fix_identities(config).await?;
        possible_identities = Complete::load_all()?;
    }

    let chosen_name = if possible_identities.len() == 1 {
        possible_identities[0].clone().name
    } else if let Some(configured_key) = crate::configured_signing_key(config).await? {
        possible_identities
            .iter()
            .find(|id| id.public_key.key == configured_key.key)
            .map(|id| id.name.clone())
            .ok_or_else(|| IdentityError::SigningKeyNotInAgent)?
    } else {
        possible_identities[0].clone().name
    };

    CHOSEN_IDENTITY
        .set(chosen_name.clone())
        .expect("OnceLock::set failed after successful OnceLock::get check");

    Ok(chosen_name)
}

impl Complete {
    /// Loads a complete identity by name.
    pub fn load(identity_name: &str) -> Result<Self, IdentityError> {
        let identity_path = path(identity_name, true)?;
        let text = fs::read_to_string(identity_path.join("identity.toml"))?;
        let mut identity: Self = toml::from_str(&text)?;
        identity.name = identity_name.to_string();
        Ok(identity)
    }

    /// Loads all valid identities found on disk.
    pub fn load_all() -> Result<Vec<Self>, IdentityError> {
        let config_dir = pijul_config::global_config_directory()?;
        let identities_path = config_dir.join("identities");
        std::fs::create_dir_all(&identities_path)?;

        let identities_dir = identities_path.as_path().read_dir()?;
        let mut identities = vec![];

        for dir_entry in identities_dir {
            let file_name = dir_entry?.file_name();
            let identity_name = file_name.to_string_lossy();

            if let Ok(identity) = Self::load(&identity_name) {
                identities.push(identity);
            }
        }

        Ok(identities)
    }
}
