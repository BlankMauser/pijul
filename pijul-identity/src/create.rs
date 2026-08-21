use super::Complete;
use super::load::path;

use crate::IdentityError;

use std::io::Write;
use std::{fs, path::PathBuf};

use log::debug;

impl Complete {
    pub async fn create(&self, config: &pijul_config::Config) -> Result<(), IdentityError> {
        let identity = if self.public_key.key.is_empty() {
            let mut identity = self.clone();
            identity.public_key = crate::resolve_signing_key(config).await?;
            identity
        } else {
            self.clone()
        };
        identity.write()
    }

    pub fn replace_with(self, new_identity: Self) -> Result<Self, IdentityError> {
        let changed_names = self.name != new_identity.name;

        if changed_names {
            let old_identity_path = path(&self.name, true)?;
            debug!("Removing old directory: {old_identity_path:?}");
            fs::remove_dir_all(&old_identity_path).map_err(|e| {
                IdentityError::Io(std::io::Error::new(
                    e.kind(),
                    format!("Could not remove old identity at {old_identity_path:?}: {e}"),
                ))
            })?;

            let new_identity_path = path(&new_identity.name, false)?;
            debug!("Creating new directory: {new_identity_path:?}");
            fs::create_dir_all(&new_identity_path).map_err(|e| {
                IdentityError::Io(std::io::Error::new(
                    e.kind(),
                    format!("Could not create new identity at {new_identity_path:?}: {e}"),
                ))
            })?;

            new_identity.write()?;
        } else {
            let identity_dir = path(&new_identity.name, false)?;
            if self.config != new_identity.config || self.public_key != new_identity.public_key {
                new_identity.write_config(&identity_dir)?;
            }
        }

        Ok(new_identity)
    }

    pub(crate) fn write_config(&self, identity_dir: &PathBuf) -> Result<(), IdentityError> {
        let config_data = toml::to_string_pretty(&self)?;
        let mut config_file = std::fs::File::create(identity_dir.join("identity.toml"))?;
        config_file.write_all(config_data.as_bytes())?;
        Ok(())
    }

    fn write(&self) -> Result<(), IdentityError> {
        if let Ok(existing_identity) = Self::load(&self.name) {
            return Err(IdentityError::AlreadyExists(existing_identity.to_string()));
        }

        let identity_dir = path(&self.name, false)?;
        std::fs::create_dir_all(&identity_dir)?;
        self.write_config(&identity_dir)?;

        Ok(())
    }
}
