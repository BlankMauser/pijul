pub mod author;
pub mod error;
pub mod global;
pub mod hook;
pub mod local;
pub mod remote;
pub mod template;

pub use error::ConfigError;

use author::Author;
use global::Global;
use hook::Hooks;
use local::Local;
use remote::RemoteConfig;
use template::Template;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dialoguer::theme;
use figment::Figment;
use figment::providers::{Format, Toml};
use log::{info, warn};
use serde::*;

pub const DEFAULT_CONFIG: &str = include_str!("defaults.toml");
pub const REPOSITORY_CONFIG_FILE: &str = "config";
pub const GLOBAL_CONFIG_FILE: &str = ".pijulconfig";
pub const CONFIG_DIR: &str = "pijul";
pub const CONFIG_FILE: &str = "config.toml";
/// Tracked, repository-wide configuration shared by every author, at the repo
/// root. Unlike `.pijul/config.toml` (personal, untracked) this file is
/// versioned and travels with the repository.
pub const SHARED_CONFIG_FILE: &str = "pijul.toml";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Shared {
    pub unrecord_changes: Option<usize>,
    pub reset_overwrites_changes: Option<Choice>,
    pub colors: Option<Choice>,
    pub pager: Option<Choice>,
    pub template: Option<Template>,
    #[serde(default)]
    pub hooks: Hooks,
    /// SSH public key to use for signing — fingerprint (`SHA256:…`) or path to a `.pub` file.
    /// Set this in `~/.config/pijul/config.toml` or `.pijul/config.toml` to skip interactive key selection.
    pub signing_key: Option<String>,
}

/// Monorepo boundaries: top-level directories (or imported sub-roots) that
/// should stay separable. Lives in the tracked `pijul.toml` so the whole team
/// shares the same map; `clone --into` maintains it, and `record` refuses a
/// `FileMove` that crosses a boundary unless `--force`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Monorepo {
    #[serde(default)]
    pub boundaries: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    // Store a copy of the original files, so that they can be modified independently
    #[serde(skip)]
    global_config: Option<Global>,
    #[serde(skip)]
    local_config: Option<Local>,
    #[serde(skip)]
    repo_root: Option<PathBuf>,
    /// Hooks declared in the tracked, repository-wide `pijul.toml`. Kept out of
    /// the figment merge (and thus out of `hooks` below) so they can be ordered
    /// and, being versioned code shared with everyone, gated behind approval.
    #[serde(skip)]
    pub shared_hooks: hook::Hooks,
    /// Monorepo boundaries declared in the tracked `pijul.toml`. Like
    /// `shared_hooks`, kept out of the figment merge so a personal
    /// `.pijul/config.toml` cannot silently weaken enforcement — the boundary
    /// map is a shared, team-wide invariant.
    #[serde(skip)]
    pub monorepo: Monorepo,

    // Global
    #[serde(default)]
    pub author: Author,
    pub ignore_kinds: HashMap<String, Vec<String>>,

    // Local
    pub default_remote: Option<String>,
    pub extra_dependencies: Vec<String>,
    #[serde(default)]
    pub remotes: Vec<RemoteConfig>,
    #[serde(default)]
    pub pins: Vec<String>,

    // Shared
    pub unrecord_changes: Option<usize>,
    pub reset_overwrites_changes: Choice,
    pub colors: Choice,
    pub pager: Choice,
    pub template: Option<Template>,
    #[serde(default)]
    pub hooks: hook::Hooks,
    pub signing_key: Option<String>,
}

impl Config {
    pub fn load(
        repository_path: Option<&Path>,
        config_overrides: Vec<(String, String)>,
    ) -> Result<Self, ConfigError> {
        let global_config = match Global::config_file() {
            Some(global_config_path) => match Global::read_contents(&global_config_path) {
                Ok(contents) => Some((global_config_path, contents)),
                Err(error) => {
                    warn!("Unable to read global config file: {error:#?}");
                    None
                }
            },
            None => {
                warn!("Unable to find global configuration path");
                None
            }
        };

        let local_config = match repository_path {
            Some(repository_path) => {
                let config_path = Local::config_file(repository_path);
                match Local::read_contents(&config_path) {
                    Ok(contents) => Some((config_path, contents)),
                    Err(error) => {
                        warn!("Unable to read local config file: {error:#?}");
                        None
                    }
                }
            }
            None => {
                info!(
                    "Skipping local configuration path - repository path was not supplied by caller"
                );
                None
            }
        };

        // Repository-wide shared config (tracked `pijul.toml` at the repo root).
        let shared_config = match repository_path {
            Some(repository_path) => {
                let config_path = repository_path.join(SHARED_CONFIG_FILE);
                match std::fs::read_to_string(&config_path) {
                    Ok(contents) => Some((config_path, contents)),
                    Err(error) => {
                        // A missing shared config is the common case, not an error.
                        if error.kind() != std::io::ErrorKind::NotFound {
                            warn!("Unable to read shared config file: {error:#?}");
                        }
                        None
                    }
                }
            }
            None => None,
        };

        Self::load_with_shared(
            global_config,
            local_config,
            shared_config,
            repository_path.map(|p| p.to_path_buf()),
            config_overrides,
        )
    }

    pub fn load_with(
        global_config_file: Option<(PathBuf, String)>,
        local_config_file: Option<(PathBuf, String)>,
        config_overrides: Vec<(String, String)>,
    ) -> Result<Self, ConfigError> {
        Self::load_with_shared(
            global_config_file,
            local_config_file,
            None,
            None,
            config_overrides,
        )
    }

    pub fn load_with_shared(
        global_config_file: Option<(PathBuf, String)>,
        local_config_file: Option<(PathBuf, String)>,
        shared_config_file: Option<(PathBuf, String)>,
        repo_root: Option<PathBuf>,
        config_overrides: Vec<(String, String)>,
    ) -> Result<Self, ConfigError> {
        // Merge the two configuration values, using the raw TOML string instead of the deserialized structs.
        // Figment uses a dictionary to store which fields are set, and using an already-deserialized
        // struct will guarantee that each layer will override the previous one.
        //
        // For example, if the optional `unrecord_changes` field is set as 1 globally but not set locally:
        // - Using deserialized structs (incorrect behaviour):
        //      - Global config is set to Some(1)
        //      - Local config is set to None - no value was found, so serde inserted the default
        //      - The local config technically has a value set, so the final (incorrect) value is None
        // - Using strings (correct behaviour):
        //      - Global config is set to Some(1)
        //      - Local config is unset
        //      - The final (correct) value is Some(1)
        let mut layers = Figment::new();

        // 1. Included defaults (defaults.toml)
        layers = layers.merge(Toml::string(DEFAULT_CONFIG));

        // 1.5 Shared repository config (tracked `pijul.toml`). It is the weakest
        // real layer — global and local prefs override it. Its `[hooks]` are
        // pulled out into `shared_hooks` (ordered and approval-gated separately)
        // and stripped from the layer so they never enter the personal `hooks`.
        let mut shared_hooks = hook::Hooks::default();
        let mut shared_monorepo = Monorepo::default();
        if let Some((_path, contents)) = shared_config_file {
            let mut value: toml::Value = toml::from_str(&contents)?;
            if let Some(table) = value.as_table_mut() {
                if let Some(hooks_value) = table.remove("hooks") {
                    shared_hooks = hooks_value.try_into()?;
                }
                // Boundaries are a shared invariant: pull them out of the figment
                // merge so no personal layer can override (weaken) them.
                if let Some(monorepo_value) = table.remove("monorepo") {
                    shared_monorepo = monorepo_value.try_into()?;
                }
            }
            layers = layers.merge(Toml::string(&toml::to_string(&value)?));
        }

        // 2. Global config
        let global_config = match global_config_file {
            Some((path, contents)) => {
                // Parse the config (and make sure it's valid!)
                let global_config = Global::parse_contents(&path, &contents)?;
                // Add the configuration layer as a string
                layers = layers.merge(Toml::string(&contents));

                Some(global_config)
            }
            None => None,
        };

        // 3. Local config
        let local_config = match local_config_file {
            Some((path, contents)) => {
                // Parse the config (and make sure it's valid!)
                let global_config = Local::parse_contents(&path, &contents)?;
                // Add the configuration layer as a string
                layers = layers.merge(Toml::string(&contents));

                Some(global_config)
            }
            None => None,
        };

        // 4. Command-line configuration overrides
        for (key, value) in config_overrides {
            layers = layers.join((key, value));
        }

        // Extract the configuration
        let mut config: Self = layers.extract()?;

        // These fields are annotated with #[serde(skip)] and therefore should be None
        assert!(config.global_config.is_none());
        assert!(config.local_config.is_none());

        // Store the original configuration sources so they can be modified later
        config.global_config = global_config;
        config.local_config = local_config;
        config.repo_root = repo_root;
        config.shared_hooks = shared_hooks;
        config.monorepo = shared_monorepo;

        Ok(config)
    }

    /// The declared monorepo boundaries (from the tracked `pijul.toml`). The thin
    /// interface consumed by `record` (crossing-move guard) and `clone --into`.
    pub fn boundaries(&self) -> &[String] {
        &self.monorepo.boundaries
    }

    /// Fingerprint of the shared hooks: a BLAKE3 hash of their canonical
    /// serialisation. Approval stores this; `record` compares against it, so any
    /// edit to the shared hooks (in `pijul.toml`) invalidates the approval.
    /// BLAKE3 is collision-resistant, so a crafted `pijul.toml` cannot forge a
    /// fingerprint that matches a previously approved one.
    pub fn shared_hooks_fingerprint(&self) -> Result<String, ConfigError> {
        let canonical = toml::to_string(&self.shared_hooks)?;
        Ok(blake3::hash(canonical.as_bytes()).to_hex().to_string())
    }

    /// Whether the shared hooks may run. Vacuously true when there are none;
    /// otherwise true only when the approval stored in `.pijul/config.toml`
    /// matches the current fingerprint. Non-interactive: an unapproved or
    /// changed set is simply not approved (callers skip it with a warning).
    pub fn shared_hooks_approved(&self) -> bool {
        if self.shared_hooks.is_empty() {
            return true;
        }
        let Ok(fingerprint) = self.shared_hooks_fingerprint() else {
            return false;
        };
        match self
            .local_config
            .as_ref()
            .and_then(|l| l.hooks_approved.as_deref())
        {
            Some(stored) => stored == fingerprint,
            None => false,
        }
    }

    /// Approve the current shared hooks so `record` will run them. The
    /// fingerprint is written into the local (untracked) `.pijul/config.toml`.
    pub fn approve_shared_hooks(&self) -> Result<(), ConfigError> {
        let fingerprint = self.shared_hooks_fingerprint()?;
        let mut local = match self.local_config.clone() {
            Some(local) => local,
            None => {
                let root = self
                    .repo_root
                    .as_ref()
                    .ok_or(ConfigError::MissingSourceFile)?;
                Local::new(root)
            }
        };
        local.hooks_approved = Some(fingerprint);
        local.write()
    }

    /// Revoke any existing approval of the shared hooks.
    pub fn revoke_shared_hooks(&self) -> Result<(), ConfigError> {
        if let Some(mut local) = self.local_config.clone() {
            if local.hooks_approved.is_some() {
                local.hooks_approved = None;
                local.write()?;
            }
        }
        Ok(())
    }

    pub fn global(&self) -> Option<Global> {
        self.global_config.clone()
    }

    pub fn local(&self) -> Option<Local> {
        self.local_config.clone()
    }

    pub fn dot_ignore_contents(&self, ignore_kind: Option<&str>) -> Result<String, ConfigError> {
        let default_ignore_lines = self
            .ignore_kinds
            .get("default")
            .ok_or_else(|| ConfigError::UnknownIgnoreKind("default".into()))?;

        // Find any extra lines to add to the `.ignore`, if they exist
        let extra_ignore_lines = match ignore_kind {
            Some(kind) => match self.ignore_kinds.get(kind) {
                Some(extra_ignore_lines) => extra_ignore_lines.iter(),
                None => {
                    return Err(ConfigError::UnknownIgnoreKind(kind.to_string()));
                }
            },
            None => [].iter(),
        };

        // Merge the default and specific ignore lines
        let mut ignore_lines = default_ignore_lines
            .iter()
            .chain(extra_ignore_lines)
            .map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        // Add a newline at the end of the file
        if !ignore_lines.is_empty() {
            ignore_lines.push('\n');
        }

        Ok(ignore_lines)
    }

    /// Choose the right dialoguer theme based on user's config
    pub fn theme(&self) -> Box<dyn theme::Theme + Send + Sync> {
        match self.colors {
            Choice::Auto | Choice::Always => Box::new(theme::ColorfulTheme::default()),
            Choice::Never => Box::new(theme::SimpleTheme),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Choice {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "always")]
    Always,
    #[serde(rename = "never")]
    Never,
}

/// Select which configuration directory to use
pub fn global_config_directory() -> Result<PathBuf, ConfigError> {
    // 1. $PIJUL_CONFIG_DIR/  (used as-is: this is the config *directory*, so
    //    callers can `.join(CONFIG_FILE)` / `.join("identities")` themselves)
    std::env::var("PIJUL_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        // 2. ~/.config/pijul/
        .or_else(|| {
            dirs_next::config_dir().map(|global_config_dir| global_config_dir.join(CONFIG_DIR))
        })
        // 3. ~/.pijulconfig/
        .or_else(|| dirs_next::home_dir().map(|home_dir| home_dir.join(CONFIG_DIR)))
        .ok_or(ConfigError::ConfigDirNotFound)
}

/// Append `dir` to `[monorepo] boundaries` in the tracked `pijul.toml` at
/// `repo_root`, creating the file and/or section if needed. Idempotent: a
/// boundary already present is left untouched. Returns `true` iff the file was
/// modified. Used by `clone --into` to register the imported sub-root as a
/// shared boundary.
pub fn add_boundary_to_shared(repo_root: &Path, dir: &str) -> Result<bool, ConfigError> {
    let path = repo_root.join(SHARED_CONFIG_FILE);
    let mut value: toml::Value = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(Default::default())
        }
        Err(e) => return Err(ConfigError::Io(e)),
    };
    let table = value
        .as_table_mut()
        .ok_or_else(|| ConfigError::MalformedShared("top level is not a table".into()))?;
    let monorepo = table
        .entry("monorepo".to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let monorepo = monorepo
        .as_table_mut()
        .ok_or_else(|| ConfigError::MalformedShared("`monorepo` is not a table".into()))?;
    let boundaries = monorepo
        .entry("boundaries".to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let boundaries = boundaries
        .as_array_mut()
        .ok_or_else(|| ConfigError::MalformedShared("`boundaries` is not an array".into()))?;
    if boundaries.iter().any(|v| v.as_str() == Some(dir)) {
        return Ok(false);
    }
    boundaries.push(toml::Value::String(dir.to_string()));
    std::fs::write(&path, toml::to_string(&value)?)?;
    Ok(true)
}

/// Parse a command-line configuration argument into a key/value pair
pub fn parse_config_arg(argument: &str) -> Result<(String, String), ConfigError> {
    let (key, value) = argument
        .split_once('=')
        .ok_or(ConfigError::InvalidConfigArg)?;

    Ok((key.to_string(), value.to_string()))
}
