use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Config(#[from] pijul_config::ConfigError),
    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),
    #[error(transparent)]
    TomlDe(#[from] toml::de::Error),
    #[error("SSH agent error: {0}")]
    SshAgent(String),
    #[error("Identity '{0}' already exists")]
    AlreadyExists(String),
    #[error("No identity found at {0}")]
    NotFound(PathBuf),
    #[error("No SSH keys in agent; add one with `ssh-add` then run `pijul identity new`")]
    NoSshKeys,
    #[error("Multiple SSH keys in agent; configure `signing_key` in config:\n{0}")]
    MultipleSshKeys(String),
    #[error("No key in SSH agent matches the configured signing_key")]
    SigningKeyNotInAgent,
    #[error("Cannot parse public key: {0}")]
    PublicKeyParse(String),
    #[error("sshsig serialization failed: {0}")]
    SshSigSerialize(String),
}
