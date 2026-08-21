//! Tests for the tracked, repository-wide `pijul.toml` config layer and its
//! approval-gated shared hooks.

use pijul_config::Config;
use std::path::PathBuf;

const SHARED_CONFIG: &str = r#"
default_remote = "shared-remote"

[hooks]
preHooks = [ "echo pre" ]
record = [ "cargo fmt" ]
"#;

fn shared(contents: &str) -> Option<(PathBuf, String)> {
    Some((PathBuf::from("pijul.toml"), String::from(contents)))
}

/// The shared `[hooks]` are pulled into `shared_hooks` and kept out of the
/// personal `hooks`; non-hook shared fields flow into the merged config.
#[test]
fn shared_hooks_are_separated() -> Result<(), anyhow::Error> {
    let config = Config::load_with_shared(None, None, shared(SHARED_CONFIG), None, Vec::new())?;

    assert_eq!(config.shared_hooks.pre.len(), 1);
    assert_eq!(config.shared_hooks.record.len(), 1);
    // Shared hooks must not leak into the personal hook set.
    assert!(config.hooks.is_empty());
    // Non-hook shared prefs are applied as the weakest layer.
    assert_eq!(config.default_remote.as_deref(), Some("shared-remote"));

    Ok(())
}

/// The shared `pijul.toml` is the weakest layer: personal (local) config wins.
#[test]
fn local_overrides_shared() -> Result<(), anyhow::Error> {
    let local = Some((PathBuf::new(), String::from(r#"default_remote = "mine""#)));
    let config = Config::load_with_shared(None, local, shared(SHARED_CONFIG), None, Vec::new())?;

    assert_eq!(config.default_remote.as_deref(), Some("mine"));

    Ok(())
}

/// With no shared hooks declared, approval is vacuously satisfied.
#[test]
fn no_shared_hooks_is_approved() -> Result<(), anyhow::Error> {
    let config = Config::load_with_shared(
        None,
        None,
        shared("default_remote = \"x\""),
        None,
        Vec::new(),
    )?;

    assert!(config.shared_hooks.is_empty());
    assert!(config.shared_hooks_approved());

    Ok(())
}

/// Full approval round-trip: unapproved -> `approve` writes the fingerprint into
/// the untracked local config -> a reload sees it approved -> editing the shared
/// hooks invalidates the approval automatically.
#[test]
fn approval_round_trip() -> Result<(), anyhow::Error> {
    let repo = tempfile::tempdir()?;
    let root = repo.path();
    std::fs::create_dir(root.join(pijul_core::DOT_DIR))?;

    let load = |root: &std::path::Path, hooks: &str| -> Result<Config, anyhow::Error> {
        // Read the personal config back if `approve` has written it.
        let local_path = root.join(pijul_core::DOT_DIR).join("config.toml");
        let local = std::fs::read_to_string(&local_path)
            .ok()
            .map(|contents| (local_path, contents));
        Ok(Config::load_with_shared(
            None,
            local,
            shared(hooks),
            Some(root.to_path_buf()),
            Vec::new(),
        )?)
    };

    // Declared but not yet approved.
    let config = load(root, SHARED_CONFIG)?;
    assert!(!config.shared_hooks_approved());

    // Approve, then a fresh load reads the approval from `.pijul/config.toml`.
    config.approve_shared_hooks()?;
    let config = load(root, SHARED_CONFIG)?;
    assert!(config.shared_hooks_approved());

    // Editing the shared hooks changes their fingerprint, invalidating approval.
    let edited = SHARED_CONFIG.replace("cargo fmt", "rm -rf /");
    let config = load(root, &edited)?;
    assert!(!config.shared_hooks_approved());

    // Revoking clears the stored approval even for the originally-approved set.
    let config = load(root, SHARED_CONFIG)?;
    assert!(config.shared_hooks_approved());
    config.revoke_shared_hooks()?;
    let config = load(root, SHARED_CONFIG)?;
    assert!(!config.shared_hooks_approved());

    Ok(())
}
