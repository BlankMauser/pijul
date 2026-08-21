use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ConfigError;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Hooks {
    /// Hooks that run *before* the shared and personal `record` hooks. This is
    /// the escape hatch that lets a personal hook run ahead of the shared ones
    /// (which otherwise run first). Serialised under the `preHooks` key.
    #[serde(default, rename = "preHooks", skip_serializing_if = "Vec::is_empty")]
    pub pre: Vec<HookEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub record: Vec<HookEntry>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.record.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookEntry(toml::Value);

#[derive(Debug, Serialize, Deserialize)]
struct RawHook {
    command: String,
    args: Vec<String>,
}

impl HookEntry {
    /// Whether this hook references the `$FILE` variable, i.e. it wants to be
    /// run once per changed file (with `$FILE` set to that file's path) rather
    /// than once for the whole repository. Cheap textual check; the shell does
    /// the actual expansion.
    /// A human-readable one-line rendering of the hook, for `pijul hooks show`.
    pub fn display(&self) -> String {
        match &self.0 {
            toml::Value::String(s) => s.clone(),
            v => match v.clone().try_into::<RawHook>() {
                Ok(h) if h.args.is_empty() => h.command,
                Ok(h) => format!("{} {}", h.command, h.args.join(" ")),
                Err(_) => v.to_string(),
            },
        }
    }

    pub fn is_per_file(&self) -> bool {
        let mentions = |s: &str| s.contains("$FILE") || s.contains("${FILE}");
        match &self.0 {
            toml::Value::String(s) => mentions(s),
            v => match v.clone().try_into::<RawHook>() {
                Ok(hook) => mentions(&hook.command) || hook.args.iter().any(|a| mentions(a)),
                Err(_) => false,
            },
        }
    }

    /// Run the hook. When `file` is `Some`, the `FILE` environment variable is
    /// set to that path so a `$FILE`-style command formats just that file; the
    /// shell (string form) expands it. See [`HookEntry::is_per_file`].
    pub fn run(&self, path: PathBuf, file: Option<&str>) -> Result<(), ConfigError> {
        let with_file = |cmd: &mut std::process::Command| {
            if let Some(f) = file {
                cmd.env("FILE", f);
            }
        };
        let (proc, s) = match &self.0 {
            toml::Value::String(s) => {
                if s.is_empty() {
                    return Ok(());
                }
                (
                    if cfg!(target_os = "windows") {
                        let mut cmd = std::process::Command::new("cmd");
                        cmd.current_dir(path).args(["/C", s]);
                        with_file(&mut cmd);
                        cmd.output()?
                    } else {
                        let mut cmd = std::process::Command::new(
                            std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()),
                        );
                        cmd.current_dir(path).arg("-c").arg(s);
                        with_file(&mut cmd);
                        cmd.output()?
                    },
                    s.clone(),
                )
            }
            v => {
                let hook = v.clone().try_into::<RawHook>()?;
                let mut cmd = std::process::Command::new(&hook.command);
                cmd.current_dir(path).args(&hook.args);
                with_file(&mut cmd);
                (cmd.output()?, hook.command)
            }
        };
        if !proc.status.success() {
            let mut stderr = std::io::stderr();
            writeln!(stderr, "Hook {:?} exited with code {:?}", s, proc.status)?;
            std::process::exit(proc.status.code().unwrap_or(1))
        }
        Ok(())
    }
}
