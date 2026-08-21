use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::bail;
use canonical_path::CanonicalPathBuf;
use clap::{Parser, ValueHint};
use log::*;

use crate::commands::common_opts::RepoPath;
use crate::commands::load_channel;
use pijul_core::pristine::{
    ChangeId, ChannelMutTxnT, Position,
    sanakirja::{MutTxn, RawMutTxnT},
};
use pijul_core::{ArcTxn, ChannelRef, ChannelTxnT, DepsTxnT, MutTxnT, TxnT, TxnTExt};
use pijul_interaction::{OUTPUT_MESSAGE, Spinner};
use pijul_repository::Repository;

#[derive(Parser, Debug)]
pub struct Reset {
    #[clap(flatten)]
    pub base: RepoPath,
    /// Reset the working copy to this channel, and change the current channel to this channel.
    #[clap(long = "channel")]
    pub channel: Option<String>,
    /// Print this file to the standard output, without modifying the repository (works for a single file only).
    #[clap(long = "dry-run")]
    pub dry_run: bool,
    /// Only reset these files
    #[clap(value_hint = ValueHint::FilePath)]
    pub files: Vec<PathBuf>,
}

impl Reset {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let overwrite_changes = match config.reset_overwrites_changes {
            pijul_config::Choice::Never => false,
            pijul_config::Choice::Auto | pijul_config::Choice::Always => true,
        };

        self.reset(overwrite_changes)
    }

    pub fn switch(self) -> Result<(), anyhow::Error> {
        self.reset(false)
    }

    fn reset(mut self, overwrite_changes: bool) -> Result<(), anyhow::Error> {
        use std::io::Write;
        let mut stderr = std::io::stderr();

        let has_repo_path = self.base.repo_path().is_some();
        let repo = self.base.find_root()?;
        let txn = repo.pristine.arc_txn_begin()?;

        let repo_path = CanonicalPathBuf::canonicalize(&repo.path)?;

        let (channel, _) = load_channel(self.channel.as_deref(), &*txn.read())?;

        if self.dry_run {
            if self.files.len() != 1 {
                bail!("reset --dry-run needs exactly one file");
            }
            // Tolerant path resolution (see the note in the per-file branch
            // below): `reset --dry-run` prints a file straight from the
            // pristine, so the working-copy file need not exist.
            let path = if has_repo_path {
                repo_relative_path(repo_path.as_ref(), &repo.path, &self.files[0])?
            } else {
                repo_relative_path(
                    repo_path.as_ref(),
                    &std::env::current_dir()?,
                    &self.files[0],
                )?
            };
            if path.is_empty() || !txn.read().is_tracked(&path)? {
                bail!("path not tracked by Pijul: {}", self.files[0].display());
            }
            let (pos, _ambiguous) =
                txn.read()
                    .follow_oldest_path(&repo.changes, &channel, &path)?;
            pijul_core::output::output_file(
                &repo.changes,
                &txn,
                &channel,
                pos,
                &mut pijul_core::vertex_buffer::Writer::new(std::io::stdout()),
            )?;
            return Ok(());
        }

        let current_channel = txn
            .read()
            .current_channel()
            .unwrap_or(pijul_core::DEFAULT_CHANNEL)
            .to_string();
        if self.channel.as_deref() == Some(&current_channel) {
            if !overwrite_changes {
                return Ok(());
            }
        } else if self.channel.is_some() {
            if !self.files.is_empty() {
                bail!("Cannot use --channel with individual paths. Did you mean --dry-run?")
            }
            let channel = {
                let txn = txn.read();
                txn.load_channel(current_channel.parse()?)?
            };
            if let Some(channel) = channel {
                if has_unrecorded_changes(txn.clone(), channel.clone(), &repo)? {
                    bail!("Cannot change channel, as there are unrecorded changes.")
                }
            }
        }

        let now = std::time::Instant::now();
        let mut conflicts = Vec::new();
        if self.files.is_empty() {
            if self.channel.is_none() || self.channel.as_deref() == Some(&current_channel) {
                if !overwrite_changes
                    && has_unrecorded_changes(txn.clone(), channel.clone(), &repo)?
                {
                    bail!(
                        "Refusing to reset, as there are unrecorded changes. Set reset_overwrites_changes = \"always\" in config to allow."
                    )
                }

                let last_modified = last_modified(&*txn.read(), &*channel.read());
                pijul_core::output::output_repository_no_pending_current(
                    &repo.working_copy,
                    &repo.changes,
                    &txn,
                    &channel,
                    "",
                    true,
                    Some(last_modified),
                    std::thread::available_parallelism()?.get(),
                    0,
                    true,
                )?;
                txn.write().touch_channel(&mut *channel.write(), None);
                txn.commit()?;

                writeln!(stderr, "Reset repository to last recorded change")?;
                return Ok(());
            }
            let mut inodes = HashSet::new();
            let mut txn_ = txn.write();
            if let Some(cur) = txn_.load_channel(current_channel.parse()?)? {
                let mut changediff = HashSet::new();
                let (a, b, s) = pijul_core::pristine::last_common_state(
                    &*txn_,
                    &*cur.read(),
                    &*channel.read(),
                )?;
                let s: pijul_core::Merkle = s.into();
                debug!("last common state {:?}", s);
                let (a, b) = if s == pijul_core::Merkle::zero() {
                    (None, None)
                } else {
                    (Some(a), Some(b))
                };
                changes_after(&*txn_, &*cur.read(), a, &mut changediff, &mut inodes)?;
                changes_after(&*txn_, &*channel.read(), b, &mut changediff, &mut inodes)?;
            }

            if let Some(ref c) = self.channel {
                txn_.set_current_channel(c)?
            }
            let mut paths = BTreeSet::new();
            for pos in inodes.iter() {
                if let Some(pijul_core::fs::FindPath { path, .. }) =
                    pijul_core::fs::find_path(&repo.changes, &*txn_, &*channel.read(), false, *pos)?
                {
                    paths.insert(path.join("/"));
                } else {
                    paths.clear();
                    break;
                }
            }
            if !inodes.is_empty() && paths.is_empty() {
                paths.insert(String::from(""));
            }
            let mut last = None;
            let _output_spinner = Spinner::new(OUTPUT_MESSAGE)?;
            std::mem::drop(txn_);
            for path in paths.iter() {
                match last {
                    Some(last_path) if path.starts_with(last_path) => continue,
                    _ => (),
                }
                debug!("resetting {:?}", path);
                conflicts.extend(
                    pijul_core::output::output_repository_no_pending_current(
                        &repo.working_copy,
                        &repo.changes,
                        &txn,
                        &channel,
                        path,
                        true,
                        None,
                        std::thread::available_parallelism()?.get(),
                        0,
                        true,
                    )?
                    .into_iter(),
                );
                last = Some(path)
            }
            txn.write().touch_channel(&mut *channel.write(), None);
        } else {
            let _output_spinner = Spinner::new(OUTPUT_MESSAGE)?;
            let cwd = std::env::current_dir()?;
            for root in self.files.iter() {
                // `reset` restores a path from the pristine, so its working-copy
                // file is frequently absent (that is precisely what you are
                // asking it to bring back). Resolve the path tolerantly instead
                // of `canonicalize`, which fails on a missing file.
                let path = repo_relative_path(repo_path.as_ref(), &cwd, root)?;
                if path.is_empty() || !txn.read().is_tracked(&path)? {
                    bail!("path not tracked by Pijul: {}", root.display());
                }
                conflicts.extend(
                    pijul_core::output::output_repository_no_pending_current(
                        &repo.working_copy,
                        &repo.changes,
                        &txn,
                        &channel,
                        &path,
                        true,
                        None,
                        std::thread::available_parallelism()?.get(),
                        0,
                        true,
                    )?
                    .into_iter(),
                );
            }
        }
        super::print_conflicts(&conflicts)?;
        txn.commit()?;
        debug!("now = {:?}", now.elapsed());
        let locks = pijul_core::TIMERS.lock().unwrap();
        info!(
            "retrieve: {:?}, graph: {:?}, output: {:?}",
            locks.alive_retrieve, locks.alive_graph, locks.alive_output,
        );

        writeln!(stderr, "Reset given paths to last recorded change")?;

        Ok(())
    }
}

/// Resolve a user-supplied path to its repo-relative, slash-separated form,
/// tolerating a path whose working-copy file no longer exists (the normal case
/// when resetting a deleted file back into place). `base` is the directory a
/// relative `file` is taken against. Returns the empty string for a path that
/// lies outside the repository.
fn repo_relative_path(
    repo_path: &Path,
    base: &Path,
    file: &Path,
) -> Result<String, std::io::Error> {
    let abs = if file.is_absolute() {
        file.to_path_buf()
    } else {
        base.join(file)
    };
    let (_full, prefix) = pijul_core::working_copy::filesystem::get_prefix(Some(repo_path), &abs)?;
    Ok(prefix)
}

fn changes_after<T: ChannelTxnT + DepsTxnT>(
    txn: &T,
    chan: &T::Channel,
    from: Option<u64>,
    changediff: &mut HashSet<ChangeId>,
    inodes: &mut HashSet<Position<ChangeId>>,
) -> Result<(), anyhow::Error> {
    let f = if let Some(f) = from {
        (f + 1).into()
    } else {
        0u64.into()
    };
    for x in pijul_core::pristine::changeid_log(txn, chan, f)? {
        let (n, p) = x?;
        let n: u64 = (*n).into();
        debug!("{:?} {:?} {:?}", n, p, from);
        if changediff.insert(p.a) {
            for y in txn.iter_rev_touched_files(&p.a, None)? {
                let (uu, pos) = y?;
                debug_assert!(uu >= &p.a);
                if uu > &p.a {
                    break;
                }
                inodes.insert(*pos);
            }
        }
    }
    Ok(())
}

fn last_modified<T: ChannelTxnT>(txn: &T, channel: &T::Channel) -> std::time::SystemTime {
    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(txn.last_modified(channel))
}

fn has_unrecorded_changes<T: RawMutTxnT + Sync + 'static>(
    txn: ArcTxn<MutTxn<T>>,
    channel: ChannelRef<MutTxn<T>>,
    repo: &Repository,
) -> Result<bool, anyhow::Error> {
    let mut state = pijul_core::RecordBuilder::new();
    state.record(
        txn,
        pijul_core::Algorithm::default(),
        false,
        &pijul_core::DEFAULT_SEPARATOR,
        channel,
        &repo.working_copy,
        &repo.changes,
        "",
        std::thread::available_parallelism()?.get(),
    )?;
    let rec = state.finish();
    debug!("actions = {:?}", rec.actions);
    Ok(!rec.actions.is_empty())
}
