use std::path::Path;

use anyhow::bail;
use clap::Parser;
use log::*;

use crate::commands::common_opts::RepoAndChannel;
use crate::commands::load_channel;
use pijul_core::HashMap;
use pijul_core::changestore::ChangeStore;
use pijul_core::{MutTxnTExt, TxnT};
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

        // The working copy reflects the channel as it is now: once the changes
        // are applied, unrecorded changes are recorded against this fork of it.
        let base = if is_current_channel {
            Some(pijul_remote::fork_pending_base(
                &mut *txn.write(),
                &channel,
            )?)
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
        // Everything applied below, dependencies included, gets a number from
        // here on: that, not `hashes`, is what the output must cover.
        let first_applied = {
            use pijul_core::ChannelTxnT;
            txn.read().apply_counter(&*channel.read())
        };
        let mut replaced = std::collections::BTreeSet::new();
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
                // itself, so this runs with no write-guard held.) The paths of
                // the predecessors it unrecords are output too.
                replaced.extend(pijul_remote::unrecord_superseded(
                    &mut *txnw,
                    &repo.changes,
                    &channel,
                    &[pijul_remote::CS::Change(*hash)],
                )?);
                {
                    let mut channelw = channel.write();
                    txnw.apply_change_rec(&repo.changes, &mut channelw, hash)?;
                }
            }
        }

        if let Some(base) = base {
            let applied: Vec<_> = {
                use pijul_core::GraphTxnT;
                let txn_ = txn.read();
                let channel_ = channel.read();
                let mut applied = Vec::new();
                for x in
                    pijul_core::pristine::changeid_log(&*txn_, &*channel_, first_applied.into())?
                {
                    let (_, p) = x?;
                    let h: pijul_core::Hash = txn_.get_external(&p.a)?.into();
                    applied.push(pijul_remote::CS::Change(h))
                }
                applied
            };
            let touched = pijul_remote::touched_inodes(&*txn.read(), &channel, &applied)?;
            // Where the output can overwrite unrecorded changes, as the working
            // copy still is: the files these changes create have no such place.
            let prefixes = pijul_remote::touched_prefixes(
                &*txn.read(),
                &repo.changes,
                base.channel(),
                &touched,
            )?;
            debug!("touched prefixes {:?}", prefixes);
            let _output_spinner = Spinner::new(OUTPUT_MESSAGE)?;

            {
                let mut state = pijul_core::RecordBuilder::new();
                prefixes.record(
                    &mut state,
                    &txn,
                    base.channel(),
                    &repo.working_copy,
                    &repo.changes,
                )?;
                let rec = state.finish();
                if !rec.actions.is_empty() {
                    debug!("actions {:#?}", rec.actions);
                    bail!("Applying this patch would delete unrecorded changes, aborting")
                }
            }

            // Nothing is unrecorded there, but any output drops the files added
            // and never recorded from the tree, unless a pending patch holds them.
            let (hash, mut covered) = pijul_remote::pending_touched(
                txn.clone(),
                Some(base),
                &channel,
                &repo.working_copy,
                &repo.changes,
                &Default::default(),
                &replaced,
            )?;
            covered.extend(prefixes);
            let conflicts = pijul_remote::output_touched(
                &repo.working_copy,
                &repo.changes,
                &txn,
                &channel,
                &touched,
                &replaced,
                &covered,
            )?;
            super::print_conflicts(&conflicts)?;
            if let Some(h) = hash {
                let mut touched_inodes = pijul_core::unrecord::TouchedInodes::new();
                txn.write()
                    .unrecord(&repo.changes, &channel, &h, 0, &mut touched_inodes)?;
                // The pending patch is ephemeral: drop its change file.
                repo.changes.del_change(&h)?;
            }
        }
        txn.commit()?;
        Ok(())
    }
}
