use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use canonical_path::CanonicalPathBuf;
use clap::Parser;
use pijul_core::change::{ChangeHeader, Hunk, Local, LocalChange};
use pijul_core::{MutTxnT, TxnT};
use serde_derive::Serialize;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

use crate::commands::common_opts::{OutputFormat, RepoAndChannel};
use crate::commands::get_channel;

#[derive(Parser, Debug)]
pub struct Status {
    #[clap(flatten)]
    pub base: RepoAndChannel,
    /// Output format. `json` emits a structured status instead of the default
    /// human-readable summary.
    #[clap(long = "output-format", value_enum)]
    pub output_format: Option<OutputFormat>,
    /// Only show status for these paths
    pub prefixes: Vec<PathBuf>,
}

/// Structured `status --output-format json` output.
#[derive(Serialize)]
struct StatusJson {
    /// The channel the working copy is on, if any.
    channel: Option<String>,
    /// Files with unrecorded changes.
    changes: Vec<FileStatus>,
    /// Tracked-directory files that are not yet tracked.
    untracked: Vec<String>,
}

#[derive(Serialize)]
struct FileStatus {
    path: String,
    /// The kind(s) of change affecting this file ("modified", "new file", …).
    status: Vec<&'static str>,
    /// 1-based starting line of each hunk touching this file (empty for
    /// whole-file operations such as add/delete/move).
    lines: Vec<usize>,
}

impl Status {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;

        let current_channel = {
            let txn = repo.pristine.txn_begin()?;
            txn.current_channel().ok().map(|c| c.to_string())
        };

        let txn = repo.pristine.arc_txn_begin()?;
        let channel_name = get_channel(self.base.channel(), &*txn.read()).0.to_string();
        let channel_sm: pijul_core::small_string::SmallString = channel_name.parse()?;
        let channel = txn.write().open_or_create_channel(&channel_sm)?;

        // Record working copy state
        let mut state = pijul_core::RecordBuilder::new();
        if self.prefixes.is_empty() {
            state.record(
                txn.clone(),
                pijul_core::Algorithm::default(),
                false,
                &pijul_core::DEFAULT_SEPARATOR,
                channel.clone(),
                &repo.working_copy,
                &repo.changes,
                "",
                std::thread::available_parallelism()?.get(),
            )?;
        } else {
            self.fill_relative_prefixes()?;
            repo.working_copy.record_prefixes(
                txn.clone(),
                pijul_core::Algorithm::default(),
                channel.clone(),
                &repo.changes,
                &mut state,
                CanonicalPathBuf::canonicalize(&repo.path)?,
                &self.prefixes,
                false,
                std::thread::available_parallelism()?.get(),
                0,
            )?;
        }
        let rec = state.finish();
        let actions: Vec<_> = {
            let txn_ = txn.read();
            rec.actions
                .into_iter()
                .map(|r| r.globalize(&*txn_).unwrap())
                .collect()
        };
        let contents = if let Ok(c) = Arc::try_unwrap(rec.contents) {
            c.into_inner()
        } else {
            unreachable!()
        };
        let change = LocalChange::make_change(
            &*txn.read(),
            &channel,
            actions,
            contents,
            ChangeHeader::default(),
            Vec::new(),
        )?;

        // Collect per-file labels (and per-hunk start lines) from hunks
        let mut by_file: BTreeMap<String, (BTreeSet<&'static str>, BTreeSet<usize>)> =
            BTreeMap::new();
        for hunk in change.changes.iter() {
            let (path, label) = match hunk {
                Hunk::Edit {
                    local: Local { path, .. },
                    ..
                }
                | Hunk::Replacement {
                    local: Local { path, .. },
                    ..
                } => (path.clone(), "modified"),
                Hunk::FileAdd { path, .. } => (path.clone(), "new file"),
                Hunk::FileDel { path, .. } => (path.clone(), "deleted"),
                Hunk::FileUndel { path, .. } => (path.clone(), "restored"),
                Hunk::FileMove { path, .. } => (path.clone(), "moved"),
                Hunk::SolveNameConflict { path, .. } => (path.clone(), "conflict resolved"),
                Hunk::UnsolveNameConflict { path, .. } => (path.clone(), "conflict"),
                Hunk::SolveOrderConflict {
                    local: Local { path, .. },
                    ..
                } => (path.clone(), "conflict resolved"),
                Hunk::UnsolveOrderConflict {
                    local: Local { path, .. },
                    ..
                } => (path.clone(), "conflict"),
                Hunk::ResurrectZombies {
                    local: Local { path, .. },
                    ..
                } => (path.clone(), "zombie fixed"),
                Hunk::AddRoot { .. } | Hunk::DelRoot { .. } => continue,
            };
            let entry = by_file.entry(path).or_default();
            entry.0.insert(label);
            if let Some(line) = hunk.line() {
                entry.1.insert(line);
            }
        }

        let untracked: Vec<PathBuf> =
            super::diff::untracked(&repo, txn.clone())?.collect::<Result<_, _>>()?;

        if let OutputFormat::Json = self.output_format.unwrap_or_default() {
            let status = StatusJson {
                channel: current_channel,
                changes: by_file
                    .into_iter()
                    .map(|(path, (labels, lines))| FileStatus {
                        path,
                        status: labels.into_iter().collect(),
                        lines: lines.into_iter().collect(),
                    })
                    .collect(),
                untracked: untracked
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            };
            serde_json::to_writer_pretty(std::io::stdout(), &status)?;
            println!();
            return Ok(());
        }

        let use_color = super::diff::is_colored(config);
        let mut out = StandardStream::stdout(if use_color {
            ColorChoice::Auto
        } else {
            ColorChoice::Never
        });

        writeln!(
            out,
            "On channel: {}",
            current_channel.as_deref().unwrap_or("(none)")
        )?;

        // "Changes not recorded:" section
        if !by_file.is_empty() {
            writeln!(out)?;
            out.set_color(ColorSpec::new().set_bold(true))?;
            writeln!(out, "Changes not recorded:")?;
            out.reset()?;
            out.set_color(ColorSpec::new().set_dimmed(true))?;
            writeln!(
                out,
                "  (use \"pijul record [file]...\" to record into a change)"
            )?;
            writeln!(out, "  (use \"pijul reset [file]...\" to discard changes)")?;
            out.reset()?;
            writeln!(out)?;

            let label_width = by_file
                .values()
                .flat_map(|(labels, _)| labels.iter().map(|s| s.len()))
                .max()
                .unwrap_or(0);
            for (path, (labels, _)) in &by_file {
                let label = labels.iter().cloned().collect::<Vec<_>>().join(", ");
                out.set_color(ColorSpec::new().set_fg(Some(Color::Red)))?;
                writeln!(
                    out,
                    "        {:width$}   {}",
                    label,
                    path,
                    width = label_width
                )?;
                out.reset()?;
            }
        }

        // "Untracked files:" section
        if !untracked.is_empty() {
            writeln!(out)?;
            out.set_color(ColorSpec::new().set_bold(true))?;
            writeln!(out, "Untracked files:")?;
            out.reset()?;
            out.set_color(ColorSpec::new().set_dimmed(true))?;
            writeln!(out, "  (use \"pijul add <file>...\" to track)")?;
            out.reset()?;
            writeln!(out)?;
            for path in &untracked {
                out.set_color(ColorSpec::new().set_fg(Some(Color::Red)))?;
                writeln!(out, "        {}", path.display())?;
                out.reset()?;
            }
        }

        if by_file.is_empty() && untracked.is_empty() {
            writeln!(out)?;
            writeln!(out, "Nothing to record, working copy is clean.")?;
        }

        Ok(())
    }

    fn fill_relative_prefixes(&mut self) -> Result<(), anyhow::Error> {
        let cwd = std::env::current_dir()?;
        for p in self.prefixes.iter_mut() {
            if p.is_relative() {
                *p = cwd.join(&p);
            }
        }
        Ok(())
    }
}
