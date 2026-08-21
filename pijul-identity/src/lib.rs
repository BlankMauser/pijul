//! Complete identity management.
//!
//! Pijul uses SSH public keys to attribute changes.  Signing is delegated to
//! the SSH agent, so no private-key material is ever stored on disk.
//!
//! Identity files live under:
//! ```text
//! .config/pijul/identities/<IDENTITY NAME>/identity.toml
//! ```

#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]
#![warn(clippy::cargo)]

mod create;
pub mod error;
mod load;
mod repair;

pub use error::IdentityError;
pub use load::{choose_identity_name, public_key};
pub use repair::fix_identities;

use pijul_config::author::Author;

use std::fmt::Display;
use std::path::PathBuf;

use jiff::Timestamp;
use pijul_core::key::{PublicKey, SKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdentityConfig {
    #[serde(flatten)]
    pub author: Author,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<PathBuf>,
}

impl From<Author> for IdentityConfig {
    fn from(author: Author) -> Self {
        Self {
            key_path: None,
            author,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// A complete user identity, representing the public key and user info.
pub struct Complete {
    #[serde(skip)]
    pub name: String,
    #[serde(flatten)]
    pub config: IdentityConfig,
    pub last_modified: Timestamp,
    pub public_key: PublicKey,
}

impl Complete {
    #[must_use]
    pub fn new(name: String, config: IdentityConfig, public_key: PublicKey) -> Self {
        assert!(!name.is_empty(), "Identity name cannot be empty!");
        Self {
            name,
            config,
            public_key,
            last_modified: Timestamp::now(),
        }
    }

    /// Creates a placeholder identity whose public key must be filled in by
    /// `prompt_changes` before the identity is written to disk.
    pub fn default(config: &pijul_config::Config) -> Result<Self, IdentityError> {
        Ok(Self {
            name: String::from("default"),
            config: IdentityConfig::from(config.author.clone()),
            last_modified: Timestamp::now(),
            public_key: PublicKey {
                key: String::new(),
                expires: None,
            },
        })
    }

    /// Returns an `SKey` that identifies this identity's signing key.
    #[must_use]
    pub fn skey(&self) -> SKey {
        SKey::new(self.public_key.clone())
    }

    /// Strips the identity of any device-specific information.
    #[must_use]
    pub fn as_portable(&self) -> Self {
        Self {
            name: String::new(),
            last_modified: Timestamp::now(),
            config: IdentityConfig {
                key_path: None,
                author: self.config.author.clone(),
            },
            public_key: self.public_key.clone(),
        }
    }
}

// ── Key resolution ───────────────────────────────────────────────────────────

/// Auto-resolves the signing key without user interaction.
/// Uses `signing_key` from config if set, otherwise the sole key in the SSH agent.
pub async fn resolve_signing_key(
    config: &pijul_config::Config,
) -> Result<PublicKey, IdentityError> {
    use thrussh_keys::PublicKeyBase64;
    use thrussh_keys::agent::client::AgentClient;

    if let Some(key) = configured_signing_key(config).await? {
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
        [key] => Ok(PublicKey {
            key: key.public_key_base64(),
            expires: None,
        }),
        keys => {
            let fps = keys
                .iter()
                .map(|k| format!("  {}", k.fingerprint()))
                .collect::<Vec<_>>()
                .join("\n");
            Err(IdentityError::MultipleSshKeys(fps))
        }
    }
}

/// Returns the `PublicKey` that matches `signing_key` in the config,
/// by consulting the SSH agent.  Returns `None` if `signing_key` is not set.
pub async fn configured_signing_key(
    config: &pijul_config::Config,
) -> Result<Option<PublicKey>, IdentityError> {
    use thrussh_keys::PublicKeyBase64;
    use thrussh_keys::agent::client::AgentClient;

    let Some(key_spec) = &config.signing_key else {
        return Ok(None);
    };

    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|e| IdentityError::SshAgent(e.to_string()))?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| IdentityError::SshAgent(e.to_string()))?;

    if identities.is_empty() {
        return Err(IdentityError::NoSshKeys);
    }

    for key in &identities {
        if ssh_key_matches_spec(key, key_spec)? {
            return Ok(Some(PublicKey {
                key: key.public_key_base64(),
                expires: None,
            }));
        }
    }

    Err(IdentityError::SigningKeyNotInAgent)
}

fn ssh_key_matches_spec(
    key: &thrussh_keys::key::PublicKey,
    spec: &str,
) -> Result<bool, IdentityError> {
    use thrussh_keys::PublicKeyBase64;
    if key.fingerprint() == spec {
        return Ok(true);
    }
    // Treat spec as a path to a .pub file
    let path = if let Some(rest) = spec.strip_prefix("~/") {
        dirs_next::home_dir()
            .ok_or(pijul_config::ConfigError::ConfigDirNotFound)?
            .join(rest)
    } else {
        std::path::Path::new(spec).to_path_buf()
    };
    if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        if let Some(base64) = content.split_whitespace().nth(1) {
            return Ok(key.public_key_base64() == base64);
        }
    }
    Ok(false)
}

// ── SSH-agent signing ────────────────────────────────────────────────────────

fn push_ssh_string(buf: &mut Vec<u8>, s: &[u8]) {
    buf.extend_from_slice(&(s.len() as u32).to_be_bytes());
    buf.extend_from_slice(s);
}

/// Compute the blob that the SSH agent must sign, per PROTOCOL.sshsig.
fn sshsig_payload(namespace: &str, message: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(message);
    let mut blob = Vec::new();
    blob.extend_from_slice(b"SSHSIG");
    push_ssh_string(&mut blob, namespace.as_bytes());
    push_ssh_string(&mut blob, &[]); // reserved
    push_ssh_string(&mut blob, b"sha256");
    push_ssh_string(&mut blob, &hash);
    blob
}

/// Convert the `Signature` returned by the SSH agent to its SSH wire encoding
/// (`string sig_type, string sig_bytes`), which is what `SshSig.signature`
/// expects.
fn signature_to_wire(sig: &thrussh_keys::signature::Signature) -> Vec<u8> {
    use thrussh_keys::signature::Signature;
    let mut wire = Vec::new();
    match sig {
        Signature::Ed25519(bytes) => {
            push_ssh_string(&mut wire, b"ssh-ed25519");
            push_ssh_string(&mut wire, &bytes.0[..]);
        }
        #[cfg(feature = "openssl")]
        Signature::RSA { hash, bytes } => {
            use thrussh_keys::key::SignatureHash;
            let name: &[u8] = match hash {
                SignatureHash::SHA2_256 => b"rsa-sha2-256",
                SignatureHash::SHA2_512 => b"rsa-sha2-512",
                SignatureHash::SHA1 => b"ssh-rsa",
            };
            push_ssh_string(&mut wire, name);
            push_ssh_string(&mut wire, bytes);
        }
        #[allow(unreachable_patterns)]
        _ => {}
    }
    wire
}

/// Sign `message` via the SSH agent using the key identified by `skey`,
/// returning an `SshSig` struct.
pub async fn sign_sshsig(
    skey: &SKey,
    message: &[u8],
) -> Result<thrussh_keys::SshSig, IdentityError> {
    sign_sshsig_ns(skey, "pijul", message).await
}

/// Like [`sign_sshsig`], but with an explicit SSHSIG namespace so distinct
/// ceremonies (e.g. `"pijul"` for approvals vs `"pijul-http-login"` for HTTP
/// session tokens) can't have their signatures cross-used.
pub async fn sign_sshsig_ns(
    skey: &SKey,
    namespace: &str,
    message: &[u8],
) -> Result<thrussh_keys::SshSig, IdentityError> {
    use thrussh_keys::{SshSig, parse_public_key_base64};

    let ssh_pubkey = parse_public_key_base64(&skey.public_key.key)
        .map_err(|e| IdentityError::PublicKeyParse(e.to_string()))?;

    let payload = sshsig_payload(namespace, message);

    let agent = thrussh_keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|e| IdentityError::SshAgent(e.to_string()))?;
    let (_agent, result) = agent.sign_request_signature(&ssh_pubkey, &payload).await;
    let sig = result.map_err(|e| IdentityError::SshAgent(e.to_string()))?;

    let signature_wire = signature_to_wire(&sig);

    Ok(SshSig {
        version: 1,
        public_key: ssh_pubkey,
        namespace: namespace.to_string(),
        reserved: vec![],
        hash_algorithm: "sha256".to_string(),
        signature: signature_wire,
    })
}

/// Sign `message` and return a PEM-armoured sshsig string.
pub async fn sign_pem(skey: &SKey, message: &[u8]) -> Result<String, IdentityError> {
    let sig = sign_sshsig(skey, message).await?;
    sig.to_pem()
        .map_err(|e| IdentityError::SshSigSerialize(e.to_string()))
}

// ── Display ──────────────────────────────────────────────────────────────────

impl Display for Complete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let has_username = !self.config.author.username.is_empty();
        let has_remote = !self.config.author.origin.is_empty();

        let remote_details: Option<String> = if has_username && has_remote {
            Some(format!(
                " [{}@{}]",
                self.config.author.username, self.config.author.origin
            ))
        } else if has_username {
            Some(format!(" [@{}]", self.config.author.username))
        } else if has_remote {
            Some(format!(" [:{}]", self.config.author.origin))
        } else {
            None
        };

        write!(f, "{}{}", self.name, remote_details.unwrap_or_default())
    }
}
