use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("No Pijul repository found, starting from `{0}`")]
    NotFound(PathBuf),
    #[error("Already in a Pijul repository")]
    AlreadyExists,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Pristine(#[from] pijul_core::pristine::sanakirja::SanakirjaError),
    #[error(transparent)]
    Config(#[from] pijul_config::ConfigError),
}
