use std::path::{Path, PathBuf};

use anyhow::bail;
use clap::Parser;
use log::*;

use crate::commands::common_opts::RepoAndChannel;
use crate::commands::load_channel;
use pijul_core::changestore::ChangeStore;
use pijul_core::{DepsTxnT, GraphTxnT, MutTxnTExt, TxnT};
use pijul_core::{HashMap, HashSet};
use pijul_interaction::{OUTPUT_MESSAGE, Spinner};

#[derive(Parser, Debug)]
pub struct Apply {
    #[clap(flatten)]
    base: RepoAndChannel,
    /// Only apply the dependencies of the change, not the change itself. Only applicable for a single change.
    #[clap(long = "deps-only")]
    deps_only: bool,
    /// The change that need to be applied. If this value is missing, read the change in text format on the standard input.
    change: Vec<String>,
}

impl Apply {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;

        let txn = repo.pristine.arc_txn_begin()?;

        let (channel, is_current_channel) = load_channel(self.base.channel(), &*txn.read())?;

        let mut hashes = Vec::new();
        if self.change.is_empty() {
            let mut change = std::io::BufReader::new(std::io::stdin());
            let mut change =
                pijul_core::change::Change::read(&mut change, &mut HashMap::default())?;
            hashes.push(
                repo.changes
                    .save_change(&mut change, |_, _| Ok::<_, anyhow::Error>(()))?,
            )
        }

        use pijul_core::MutTxnT;
        use rand::RngExt;
        // Forked channel before the apply, in order to check whether
        // we are overwriting a path.
        let forked = if is_current_channel {
            let forked_s: pijul_core::small_string::SmallString = rand::rng()
                .sample_iter(&rand::distr::Alphanumeric)
                .take(20)
                .map(char::from)
                .collect::<String>()
                .parse()?;
            let forked = txn.write().fork(&channel, &forked_s)?;
            Some((forked_s, forked))
        } else {
            None
        };
        for ch in self.change.iter() {
            hashes.push(if let Ok(h) = txn.read().hash_from_prefix(ch) {
                h.0
            } else {
                let change = pijul_core::change::Change::deserialize(&ch, None);
                match change {
                    Ok(mut change) => repo
                        .changes
                        .save_change(&mut change, |_, _| Ok::<_, anyhow::Error>(()))?,
                    Err(pijul_core::change::ChangeError::Io(e)) => {
                        if let std::io::ErrorKind::NotFound = e.kind() {
                            let mut changes = repo.changes_dir.clone();
                            super::find_hash(&mut changes, &ch)?
                        } else {
                            return Err(e.into());
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            })
        }
        if self.deps_only {
            if hashes.len() > 1 {
                bail!("--deps-only is only applicable to a single change")
            }
            let mut channel = channel.write();
            let hash = hashes.last().unwrap();
            txn.write()
                .apply_deps_rec(&repo.changes, &mut channel, hash)?;
        } else {
            let mut txnw = txn.write();
            for hash in hashes.iter() {
                // Honor `replaces`: supersede an amended predecessor on the
                // channel before applying — in this same txn, so a failure
                // aborts the whole thing. Shared with pull/push via the core
                // `unrecord_superseded` helper. (`unrecord` locks the channel
                // itself, so this runs with no write-guard held.)
                txnw.unrecord_superseded(&repo.changes, &channel, hash)?;
                {
                    let mut channelw = channel.write();
                    txnw.apply_change_rec(&repo.changes, &mut channelw, hash)?;
                }
            }
        }

        let mut touched = HashSet::default();
        let txn_ = txn.read();
        for d in hashes.iter() {
            if let Some(int) = txn_.get_internal(&d.into())? {
                debug!("int = {:?}", int);
                for inode in txn_.iter_rev_touched(int)? {
                    debug!("{:?}", inode);
                    let (int_, inode) = inode?;
                    if int_ < int {
                        continue;
                    } else if int_ > int {
                        break;
                    }
                    touched.insert(*inode);
                }
            }
        }
        std::mem::drop(txn_);

        if let Some((_, ref forked)) = forked {
            let mut touched_files = Vec::with_capacity(touched.len());
            let txn_ = txn.read();
            for i in touched {
                if let Some(pijul_core::fs::FindPath { path, .. }) =
                    pijul_core::fs::find_path(&repo.changes, &*txn_, &*forked.read(), false, i)?
                {
                    if !path.is_empty() {
                        touched_files.push(path.join("/"));
                        continue;
                    }
                }
                touched_files.clear();
                break;
            }
            debug!("touched files {:?}", touched_files);
            std::mem::drop(txn_);
            let _output_spinner = Spinner::new(OUTPUT_MESSAGE)?;

            {
                let mut state = pijul_core::RecordBuilder::new();
                if touched_files.is_empty() {
                    state.record(
                        txn.clone(),
                        pijul_core::Algorithm::default(),
                        false,
                        &pijul_core::DEFAULT_SEPARATOR,
                        forked.clone(),
                        &repo.working_copy,
                        &repo.changes,
                        "",
                        std::thread::available_parallelism()?.get(),
                    )?
                } else {
                    use canonical_path::CanonicalPathBuf;
                    fill_relative_prefixes(&mut touched_files)?;
                    repo.working_copy.record_prefixes(
                        txn.clone(),
                        pijul_core::Algorithm::default(),
                        forked.clone(),
                        &repo.changes,
                        &mut state,
                        CanonicalPathBuf::canonicalize(&repo.path)?,
                        &touched_files,
                        false,
                        std::thread::available_parallelism()?.get(),
                        0,
                    )?;
                }
                let rec = state.finish();
                if !rec.actions.is_empty() {
                    debug!("actions {:#?}", rec.actions);
                    bail!("Applying this patch would delete unrecorded changes, aborting")
                }
            }

            let mut conflicts = Vec::new();
            for path in touched_files.iter() {
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
                        is_current_channel,
                    )?
                    .into_iter(),
                );
            }
            super::print_conflicts(&conflicts)?;
        }
        if let Some((forked_s, forked)) = forked {
            std::mem::drop(forked);
            txn.write().drop_channel(&forked_s)?;
        }
        txn.commit()?;
        Ok(())
    }
}

fn fill_relative_prefixes(prefixes: &mut [String]) -> Result<Vec<PathBuf>, anyhow::Error> {
    let cwd = std::env::current_dir()?;
    let mut pref = Vec::new();
    for p in prefixes.iter_mut() {
        if std::path::Path::new(p).is_relative() {
            pref.push(cwd.join(&p));
        }
    }
    Ok(pref)
}
