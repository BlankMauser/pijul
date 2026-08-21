use std::path::Path;

use clap::Parser;

use pijul_core::changestore::ChangeStore;
use pijul_core::*;

use crate::commands::common_opts::RepoPath;
use crate::commands::get_channel;

#[derive(Parser, Debug)]
pub struct Change {
    #[clap(flatten)]
    base: RepoPath,
    /// Instead of the raw patch, show the change inlined in the full context
    /// of each file it touches, as a structured JSON "palimpsest" (added /
    /// removed / added-then-removed spans). Intended for editor integrations.
    #[clap(long = "context")]
    context: bool,
    /// The hash of the change to show, or an unambiguous prefix thereof
    #[clap(value_name = "HASH")]
    hash: Option<String>,
}

impl Change {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;
        let changes = repo.changes;

        // Resolve the hash and the current channel, then drop the read txn
        // (palimpsest opens its own transaction).
        let (hash, channel_name) = {
            let txn = repo.pristine.txn_begin()?;
            let (channel_name, _) = get_channel(None, &txn);
            let channel_name = channel_name.to_string();
            let hash = if let Some(hash) = self.hash.take() {
                if let Some(h) = Hash::from_base32(hash.as_bytes()) {
                    h
                } else {
                    txn.hash_from_prefix(&hash)?.0
                }
            } else {
                let Some(channel) = txn.load_channel(channel_name.parse()?)? else {
                    return Ok(());
                };
                let channel = channel.read();
                if let Some(h) = txn.reverse_log(&*channel, None)?.next() {
                    (h?.1).0.into()
                } else {
                    return Ok(());
                }
            };
            (hash, channel_name)
        };

        // The palimpsest context view: the change inlined in each file's full
        // content, as a renderer-agnostic segment stream (added / removed /
        // added-then-removed). Emitted as JSON for editor clients.
        if self.context {
            let resp = palimpsest::change_diff(&repo.pristine, &changes, &channel_name, hash)?;
            serde_json::to_writer_pretty(std::io::stdout(), &resp)?;
            println!();
            return Ok(());
        }

        let change = changes.get_change(&hash)?;
        let colors = super::diff::is_colored(config);
        change.write(
            &changes,
            Some(hash),
            true,
            super::diff::Colored {
                w: termcolor::StandardStream::stdout(termcolor::ColorChoice::Auto),
                colors,
            },
        )?;
        Ok(())
    }
}
