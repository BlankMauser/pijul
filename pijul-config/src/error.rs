use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    TomlDe(#[from] toml::de::Error),
    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),
    #[error(transparent)]
    Figment(#[from] figment::Error),
    #[error("Cannot determine configuration directory")]
    ConfigDirNotFound,
    #[error("Unknown ignore kind: {0}")]
    UnknownIgnoreKind(String),
    #[error("Config file path not set")]
    MissingSourceFile,
    #[error("Invalid config argument (expected `key=value`)")]
    InvalidConfigArg,
}
