use clap::Parser;
use pijul_core::*;
use std::io::Write;
use std::path::Path;

use crate::commands::common_opts::RepoPath;
use crate::commands::get_channel;

#[derive(Parser, Debug)]
pub struct Dependents {
    #[clap(flatten)]
    base: RepoPath,
    /// The hash of the change to show, or an unambiguous prefix thereof
    #[clap(value_name = "HASH")]
    hash: Option<String>,
}

impl Dependents {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;
        let txn = repo.pristine.txn_begin()?;

        let (channel_name, _) = get_channel(None, &txn);
        let Some(channel) = txn.load_channel(channel_name.parse()?)? else {
            return Ok(());
        };

        let channelr = channel.read();

        let hash = if let Some(hash) = self.hash {
            let h = if let Some(h) = Hash::from_base32(hash.as_bytes()) {
                h
            } else {
                txn.hash_from_prefix(&hash)?.0
            };
            if txn.has_change(&channel, &h.into())?.is_none() {
                return Ok(());
            }
            h
        } else {
            return Ok(());
        };

        if let Hash::None = hash {
            eprintln!("Warning: listing dependents of the root change")
        }

        let mut ids = vec![(
            txn.get_internal(&hash.into())?
                .ok_or_else(|| anyhow::anyhow!("change not found in DB"))?,
            0u64,
            false,
        )];
        let mut seen = HashSet::new();
        let mut stdout = std::io::stdout();
        while let Some((id, n, v)) = ids.pop() {
            if v {
                let h: Hash = txn
                    .get_external(&id)
                    .optional()?
                    .ok_or_else(|| anyhow::anyhow!("change not found in DB"))?
                    .into();
                writeln!(stdout, "{}", h.to_base32())?;
            } else if seen.insert(id) {
                ids.push((id, n, true));
                let l = ids.len();
                for t in txn.iter_revdep(&id)? {
                    let (id_, t) = t?;
                    if id_ > id {
                        break;
                    }
                    if let Some(n) = txn.get_changeset(txn.changes(&channelr), t)? {
                        ids.push((t, (*n).into(), false));
                    }
                }
                (&mut ids[l..]).sort_by(|a, b| a.1.cmp(&b.1));
            }
        }
        Ok(())
    }
}
