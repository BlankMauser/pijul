use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::Parser;
use pijul_core::pristine::{Base32, ChannelRef, Hash};
use pijul_core::{MutTxnT, MutTxnTExt};

use crate::commands::common_opts::RepoPath;

/// Replays a `.pijul/replay.log` (produced by a binary built with the
/// `replay-log` feature) against a repository, re-executing every logged
/// `apply`/`unrecord`/`pending` operation in order.
///
/// The referenced changes — including the *pending* patches, which are kept on
/// disk instead of being deleted when `replay-log` is enabled — must be present
/// in `.pijul/changes`. To reproduce a bug, point this at a fresh repository
/// whose `.pijul/changes` was copied from the one that produced the log.
#[derive(Parser, Debug)]
pub struct Replay {
    #[clap(flatten)]
    base: RepoPath,
    /// The replay log to execute. Defaults to `.pijul/replay.log` in the repository.
    #[clap(long = "log", value_name = "PATH")]
    log: Option<PathBuf>,
    /// Only execute the first N operations of the log (useful for bisecting).
    #[clap(long = "count", short = 'n', value_name = "N")]
    count: Option<usize>,
}

impl Replay {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;
        let log_path = self
            .log
            .clone()
            .unwrap_or_else(|| repo.path.join(pijul_core::DOT_DIR).join("replay.log"));
        let file = std::fs::File::open(&log_path)
            .with_context(|| format!("Opening replay log {}", log_path.display()))?;
        let reader = std::io::BufReader::new(file);

        let txn = repo.pristine.arc_txn_begin()?;
        let mut channels: HashMap<String, ChannelRef<_>> = HashMap::new();
        let mut touched = pijul_core::unrecord::TouchedInodes::new();

        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(max) = self.count {
                if i >= max {
                    eprintln!("Stopping after {} operations", max);
                    break;
                }
            }
            let v: serde_json::Value =
                serde_json::from_str(&line).with_context(|| format!("Parsing log line {}", i))?;

            let op = v["op"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing `op` on line {}", i))?;
            let channel_name = v["channel"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing `channel` on line {}", i))?;
            let hash_str = v["hash"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing `hash` on line {}", i))?;
            let hash = Hash::from_base32(hash_str.as_bytes())
                .ok_or_else(|| anyhow::anyhow!("Invalid hash {:?} on line {}", hash_str, i))?;

            // Open (or reuse) the channel named in the entry.
            if !channels.contains_key(channel_name) {
                let sm: pijul_core::small_string::SmallString = channel_name.parse()?;
                let ch = txn.write().open_or_create_channel(&sm)?;
                channels.insert(channel_name.to_string(), ch);
            }
            let channel = channels.get(channel_name).unwrap();

            eprintln!("[{}] {} {} {}", i, op, channel_name, hash_str);

            match op {
                "apply" | "pending" => {
                    let mut ch = channel.write();
                    txn.write()
                        .apply_change_rec(&repo.changes, &mut ch, &hash)?;
                }
                "unrecord" | "unrecord_pending" => {
                    let salt = v["salt"].as_u64().unwrap_or(0);
                    txn.write()
                        .unrecord(&repo.changes, channel, &hash, salt, &mut touched)?;
                }
                other => bail!("Unknown operation {:?} on line {}", other, i),
            }
        }

        txn.commit()?;
        Ok(())
    }
}
