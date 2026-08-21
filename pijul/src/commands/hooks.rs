use std::io::Write;
use std::path::Path;

use clap::Parser;

use crate::commands::common_opts::RepoPath;

/// Manage the repository-wide (shared) hooks declared in the tracked
/// `pijul.toml` at the repository root. These hooks run for every author on
/// `pijul record`, but because they are versioned code shared with everyone,
/// each user must approve their current content before they will run.
#[derive(Parser, Debug)]
pub struct Hooks {
    #[clap(flatten)]
    base: RepoPath,
    #[clap(subcommand)]
    subcmd: SubCommand,
}

#[derive(Parser, Debug)]
enum SubCommand {
    /// Show the shared and personal hooks and whether the shared ones are approved
    #[clap(alias = "status")]
    Show,
    /// Approve the shared hooks currently declared in `pijul.toml` so that
    /// `pijul record` will run them. The approval is stored in `.pijul/`
    /// (untracked) and is invalidated automatically when the shared hooks change.
    Approve,
    /// Revoke a previous approval of the shared hooks
    Revoke,
}

impl Hooks {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let mut stdout = std::io::stdout();
        match self.subcmd {
            SubCommand::Show => {
                writeln!(
                    stdout,
                    "Shared hooks (from {}):",
                    pijul_config::SHARED_CONFIG_FILE
                )?;
                if config.shared_hooks.is_empty() {
                    writeln!(stdout, "  (none)")?;
                } else {
                    print_hooks(&mut stdout, &config.shared_hooks)?;
                    if config.shared_hooks_approved() {
                        writeln!(stdout, "  status: approved (will run)")?;
                    } else {
                        writeln!(
                            stdout,
                            "  status: NOT approved (skipped) — run `pijul hooks approve`"
                        )?;
                    }
                }
                writeln!(stdout, "Personal hooks (from .pijul/config.toml):")?;
                if config.hooks.is_empty() {
                    writeln!(stdout, "  (none)")?;
                } else {
                    print_hooks(&mut stdout, &config.hooks)?;
                }
            }
            SubCommand::Approve => {
                if config.shared_hooks.is_empty() {
                    writeln!(
                        stdout,
                        "No shared hooks declared in {}; nothing to approve.",
                        pijul_config::SHARED_CONFIG_FILE
                    )?;
                } else {
                    config.approve_shared_hooks()?;
                    writeln!(
                        stdout,
                        "Approved the shared hooks from {}.",
                        pijul_config::SHARED_CONFIG_FILE
                    )?;
                }
            }
            SubCommand::Revoke => {
                config.revoke_shared_hooks()?;
                writeln!(stdout, "Revoked approval of the shared hooks.")?;
            }
        }
        Ok(())
    }
}

fn print_hooks(w: &mut impl Write, hooks: &pijul_config::hook::Hooks) -> std::io::Result<()> {
    for h in hooks.pre.iter() {
        writeln!(w, "  [pre]    {}", h.display())?;
    }
    for h in hooks.record.iter() {
        writeln!(w, "  [record] {}", h.display())?;
    }
    Ok(())
}
