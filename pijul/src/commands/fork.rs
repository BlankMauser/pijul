use std::path::Path;

use clap::Parser;
use pijul_core::{Base32, ChannelTxnT, MutTxnT, MutTxnTExt, TxnT, TxnTExt};

use crate::commands::common_opts::RepoPath;
use crate::commands::load_channel;

#[derive(Parser, Debug)]
pub struct Fork {
    #[clap(flatten)]
    base: RepoPath,
    /// Make the new channel from this state instead of the current channel
    #[clap(long = "state", conflicts_with = "change", conflicts_with = "channel")]
    state: Option<String>,
    /// Make the new channel from this channel instead of the current channel
    #[clap(long = "channel", conflicts_with = "change", conflicts_with = "state")]
    channel: Option<String>,
    /// Apply this change after creating the channel
    #[clap(long = "change", conflicts_with = "channel", conflicts_with = "state")]
    change: Option<String>,
    /// The name of the new channel
    to: String,
}

impl Fork {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;
        let mut txn = repo.pristine.mut_txn_begin()?;
        let mut touched_inodes = pijul_core::unrecord::TouchedInodes::new();
        let to_sm: pijul_core::small_string::SmallString = self.to.parse()?;
        if let Some(ref ch) = self.change {
            let (hash, _) = txn.hash_from_prefix(ch)?;
            let channel = txn.open_or_create_channel(&to_sm)?;
            let mut channel = channel.write();
            txn.apply_change_rec(&repo.changes, &mut channel, &hash)?
        } else {
            let (channel, _) = load_channel(self.channel.as_deref(), &txn)?;
            let mut fork = txn.fork(&channel, &to_sm)?;

            if let Some(ref state) = self.state {
                if let Some(state) = pijul_core::Merkle::from_base32(state.as_bytes()) {
                    let ch = fork.write();
                    if let Some(n) = txn.channel_has_state(&ch.states, &state.into())? {
                        let n: u64 = n.into();

                        let mut v = Vec::new();
                        for l in txn.reverse_log(&ch, None)? {
                            let (n_, h) = l?;
                            if n_ > n {
                                v.push(h.0.into())
                            } else {
                                break;
                            }
                        }
                        std::mem::drop(ch);
                        for h in v {
                            txn.unrecord(&repo.changes, &mut fork, &h, 0, &mut touched_inodes)?;
                        }
                    }
                }
            }
        }
        txn.commit()?;
        Ok(())
    }
}
