use std::io::Write;
use std::path::Path;

use crate::commands::common_opts::RepoPath;
use crate::commands::{load_channel, load_channel_exact};
use anyhow::anyhow;
use anyhow::bail;
use clap::Parser;
use log::debug;
use pijul_core::{ChannelTxnT, MutTxnT, ResultOptionalExt, TxnT};

#[derive(Parser, Debug)]
pub struct Channel {
    #[clap(flatten)]
    base: RepoPath,
    #[clap(subcommand)]
    subcmd: Option<SubCommand>,
}

#[derive(Parser, Debug)]
pub enum SubCommand {
    /// Delete a channel.
    /// The channel must not be the current channel.
    #[clap(name = "delete")]
    Delete { delete: String },
    /// Rename a channel.
    #[clap(name = "rename", allow_missing_positional = true)]
    Rename {
        /// Name of the channel to be renamed. Defaults to current channel.
        from: Option<String>,
        /// New name of the channel to rename to.
        to: String,
    },
    /// Switch to a channel.
    /// There must not be unrecorded changes in the working copy.
    #[clap(name = "switch")]
    Switch {
        /// Name of the channel to switch to. If not specified, defaults to current channel.
        to: Option<String>,
    },
    /// Create a new, empty channel.
    #[clap(name = "new")]
    New {
        name: String,
        #[clap(long = "empty")]
        empty: bool,
    },
}

impl Channel {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self) -> Result<(), anyhow::Error> {
        let mut stdout = std::io::stdout();
        match self.subcmd {
            None => {
                let repo = self.base.find_root()?;
                let txn = repo.pristine.txn_begin()?;
                let current = txn.current_channel().ok();
                for channel in txn.channels(&pijul_core::small_string::SmallString::default())? {
                    let channel = channel.read();
                    let name = txn.name(&*channel);
                    if current == Some(name) {
                        writeln!(stdout, "* {}", name)?;
                    } else {
                        writeln!(stdout, "  {}", name)?;
                    }
                }
            }
            Some(SubCommand::Delete { ref delete }) => {
                let repo = self.base.find_root()?;
                let mut txn = repo.pristine.mut_txn_begin()?;
                let current = txn.current_channel().ok();
                if Some(delete.as_str()) == current {
                    bail!("Cannot delete current channel")
                }
                let delete_sm: pijul_core::small_string::SmallString = delete.parse()?;
                if !txn.drop_channel(&delete_sm)? {
                    return Err(anyhow!("No such channel: {}", delete));
                }
                txn.commit()?;
            }
            Some(SubCommand::Switch { to }) => {
                (crate::commands::reset::Reset {
                    base: self.base,
                    channel: to,
                    dry_run: false,
                    files: Vec::new(),
                })
                .switch()?;
            }
            Some(SubCommand::Rename { ref from, ref to }) => {
                let repo = self.base.find_root()?;
                let mut txn = repo.pristine.mut_txn_begin()?;
                let current = txn.current_channel().ok();
                let (from, to) = if let Some(from) = from {
                    (from.as_str(), to.as_str())
                } else if let Some(current) = current {
                    (current, to.as_str())
                } else {
                    bail!("No current channel")
                };

                let mut channel = load_channel_exact(from, &txn)?;

                let to_sm: pijul_core::small_string::SmallString = to.parse()?;
                txn.rename_channel(&mut channel, &to_sm)?;
                txn.set_current_channel(to)?;
                txn.commit()?;
            }
            Some(SubCommand::New { name, empty }) => {
                let repo = self.base.find_root()?;
                let mut txn = repo.pristine.mut_txn_begin()?;
                if txn.load_channel(name.parse()?)?.is_some() {
                    bail!("Channel {} already exists", name)
                }
                let name_sm: pijul_core::small_string::SmallString = name.parse()?;
                let new = txn.open_or_create_channel(&name_sm)?;
                if !empty {
                    // Safeguard: apply the root patch if we're creating a new channel.
                    let (channel, _) = load_channel(None, &txn)?;
                    let ch = channel.read();
                    use pijul_core::{GraphTxnT, MutTxnTExt};
                    let h = if let Some(Ok((k, v))) =
                        pijul_core::pristine::changeid_log(&txn, &ch, 0u64.into())?.next()
                    {
                        debug!("initial patch on current channel: {:?} {:?}", k, v);
                        Some(txn.get_external(&v.a).optional()?.unwrap().into())
                    } else {
                        None
                    };
                    if let Some(h) = h {
                        let mut new = new.write();
                        txn.apply_change(&repo.changes, &mut new, &h)?;
                    }
                }
                txn.commit()?;
            }
        }
        Ok(())
    }
}
