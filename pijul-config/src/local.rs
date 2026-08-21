use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::remote::RemoteConfig;
use crate::{CONFIG_FILE, ConfigError, REPOSITORY_CONFIG_FILE, Shared};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Local {
    #[serde(skip)]
    source_file: Option<PathBuf>,

    pub default_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_dependencies: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<RemoteConfig>,
    /// Patches that should never be pushed and always stay at the top of the
    /// channel order. Base32-encoded patch hashes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<String>,
    /// Fingerprint of the shared hooks (from the tracked `pijul.toml`) this user
    /// has approved. Personal and untracked, so pulling someone else's change
    /// cannot grant its own hooks approval. Managed via `pijul hooks approve`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_approved: Option<String>,

    #[serde(flatten)]
    pub shared_config: Shared,
}

impl Local {
    pub fn new(repository_path: &Path) -> Self {
        let config_path = Self::config_file(repository_path);
        Self {
            source_file: Some(config_path),
            ..Self::default()
        }
    }

    pub fn config_file(repository_path: &Path) -> PathBuf {
        let dot_directory = repository_path.join(pijul_core::DOT_DIR);
        let old_config_path = dot_directory.join(REPOSITORY_CONFIG_FILE);

        match old_config_path.exists() {
            true => old_config_path,
            false => dot_directory.join(CONFIG_FILE),
        }
    }

    pub fn read_contents(config_path: &Path) -> Result<String, ConfigError> {
        let mut config_file = File::open(config_path)?;
        let mut file_contents = String::new();
        config_file.read_to_string(&mut file_contents)?;
        Ok(file_contents)
    }

    pub fn parse_contents(config_path: &Path, toml_data: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(toml_data)?;
        assert!(config.source_file.is_none());
        config.source_file = Some(config_path.to_path_buf());
        Ok(config)
    }

    pub fn write(&self) -> Result<(), ConfigError> {
        let source = self
            .source_file
            .as_ref()
            .ok_or(ConfigError::MissingSourceFile)?;
        let mut config_file = File::create(source)?;
        let file_contents = toml::to_string_pretty(self)?;
        config_file.write_all(file_contents.as_bytes())?;
        Ok(())
    }
}
