use crate::{Complete, IdentityConfig, IdentityError};

use std::path::PathBuf;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum IdentityParseError {
    #[error("Could not find identity at path {0}")]
    NoIdentity(PathBuf),
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

/// Ensure that the user has at least one valid identity on disk.
///
/// Never prompts interactively.  Auto-creates a default identity using whichever
/// signing key can be resolved without input (configured key, or the sole agent
/// key).  For interactive first-time setup run `pijul identity new` instead.
pub async fn fix_identities(config: &pijul_config::Config) -> Result<(), IdentityError> {
    let mut dir = pijul_config::global_config_directory()?;
    dir.push("identities");
    std::fs::create_dir_all(&dir)?;

    if Complete::load_all()?.is_empty() {
        let public_key = resolve_key_for_auto_create(config).await?;
        let mut identity = Complete::default(config)?;
        identity.public_key = public_key;
        identity.create(config).await?;
    }

    Ok(())
}

/// Returns the public key to use when auto-creating an identity:
/// the key configured via `signing_key` in config, or the sole key in the
/// SSH agent (errors if there are multiple and none is configured).
async fn resolve_key_for_auto_create(
    config: &pijul_config::Config,
) -> Result<pijul_core::key::PublicKey, IdentityError> {
    use thrussh_keys::PublicKeyBase64;
    use thrussh_keys::agent::client::AgentClient;

    if let Some(key) = crate::configured_signing_key(config).await? {
        return Ok(key);
    }

    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|e| IdentityError::SshAgent(e.to_string()))?;
    let agent_keys = agent
        .request_identities()
        .await
        .map_err(|e| IdentityError::SshAgent(e.to_string()))?;

    match agent_keys.as_slice() {
        [] => Err(IdentityError::NoSshKeys),
        [key] => Ok(pijul_core::key::PublicKey {
            key: key.public_key_base64(),
            expires: None,
        }),
        keys => {
            let fingerprints = keys
                .iter()
                .map(|k| format!("  {}", k.fingerprint()))
                .collect::<Vec<_>>()
                .join("\n");
            Err(IdentityError::MultipleSshKeys(fingerprints))
        }
    }
}

impl Complete {
    /// Migrate from the old pre-directory identity format.
    pub fn from_old_format(_config: &pijul_config::Config) -> Result<Self, IdentityParseError> {
        let config_dir = pijul_config::global_config_directory().map_err(IdentityError::from)?;
        let secret_key_path = config_dir.join("secretkey.json");

        if !secret_key_path.exists() {
            return Err(IdentityParseError::NoIdentity(secret_key_path));
        }

        Ok(Self::new(
            String::from("default"),
            IdentityConfig::default(),
            pijul_core::key::PublicKey {
                key: String::new(),
                expires: None,
            },
        ))
    }
}
