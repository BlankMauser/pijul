use std::io::Write;
use std::path::Path;

use anyhow::bail;
use clap::Parser;
use pijul_core::changestore::ChangeStore;
use pijul_core::squash::SquashError;
use pijul_core::*;

use super::change_opts::{HeaderOpts, edit_change, sign_and_save, sub_root_names};
use super::pushpull::outgoing;
use super::{get_channel, load_channel};
use crate::commands::common_opts::RepoPath;

#[derive(Parser, Debug)]
pub struct Squash {
    #[clap(flatten)]
    base: RepoPath,
    /// Squash changes of this channel instead of the current channel
    #[clap(long = "channel")]
    channel: Option<String>,
    /// Allow the new change to span more than one sub-root (imported project)
    /// instead of erroring. The change will not commute across projects.
    #[clap(long = "force")]
    force: bool,
    #[clap(flatten)]
    header: HeaderOpts,
    /// Squash the changes that are not on this remote, i.e. the ones `pijul
    /// push` would upload (defaults to the default remote)
    #[clap(long = "remote", conflicts_with = "change_id")]
    remote: Option<Option<String>>,
    /// With --remote, compare to this remote channel (defaults to the name of
    /// the squashed channel)
    #[clap(long = "remote-channel", requires = "remote")]
    remote_channel: Option<String>,
    /// Do not check certificates (HTTPS remotes only, this option might be dangerous)
    #[clap(short = 'k', requires = "remote")]
    no_cert_check: bool,
    /// The hashes of the changes to squash (unambiguous prefixes are accepted)
    #[clap(required_unless_present = "remote")]
    change_id: Vec<String>,
}

impl Squash {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub async fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;
        let txn = repo.pristine.arc_txn_begin()?;
        let (channel, _) = load_channel(self.channel.as_deref(), &*txn.read())?;

        let mut hashes = Vec::new();
        if let Some(ref remote_name) = self.remote {
            let Some(remote_name) = remote_name.as_deref().or(config.default_remote.as_deref())
            else {
                bail!("Missing remote")
            };
            let remote_channel = if let Some(ref c) = self.remote_channel {
                c.clone()
            } else {
                get_channel(self.channel.as_deref(), &*txn.read())
                    .0
                    .to_string()
            };
            let mut remote = pijul_remote::repository(
                config,
                Some(&repo.path),
                None,
                remote_name,
                &remote_channel,
                self.no_cert_check,
                true,
            )
            .await?;
            let delta = outgoing(config, &txn, &channel, &repo, &mut remote, &[], &[]).await?;
            remote.finish().await?;
            for c in delta.to_upload {
                if let pijul_remote::CS::Change(h) = c {
                    hashes.push(h)
                }
            }
            if hashes.len() < 2 {
                // The remote cache was refreshed.
                txn.commit()?;
                writeln!(
                    std::io::stderr(),
                    "{} change(s) not on the remote, nothing to squash",
                    hashes.len()
                )?;
                return Ok(());
            }
        } else {
            let txn = txn.read();
            for c in self.change_id.iter() {
                hashes.push(txn.hash_from_prefix(c)?.0)
            }
        }
        if hashes.len() < 2 {
            bail!("Squashing needs at least two changes")
        }

        let complete =
            pijul_identity::Complete::load(&pijul_identity::choose_identity_name(config).await?)?;
        let secret = complete.skey();

        let header = self.header.header(config).await?;
        let mut squash = match pijul_core::squash::squash(
            &txn,
            &channel,
            &repo.changes,
            &hashes,
            header,
            self.force,
        ) {
            Ok(squash) => squash,
            Err(SquashError::SubRoots { sub_roots }) => {
                // The paths of the sub-roots are in the real tree.
                txn.write().end_scratch_tree()?;
                let names =
                    sub_root_names(&*txn.read(), &*channel.read(), sub_roots.iter().copied())?;
                bail!(
                    "This squash touches {} independent roots:{}\n\
                     A single change may not commute across projects.\n\
                     Re-run with --force to squash them into a single (non-commuting) change.",
                    sub_roots.len(),
                    names,
                );
            }
            Err(e) => return Err(e.into()),
        };

        if self.header.message.is_none() {
            let mut preamble = "# Squashing:\n".to_string();
            for h in squash.squashed.iter() {
                let header = repo.changes.get_header(h)?;
                preamble.push_str(&format!(
                    "#   {} {}\n",
                    h.to_base32(),
                    header.message.lines().next().unwrap_or("")
                ));
            }
            preamble.push_str("# Only the header can be edited, not the hunks.\n\n");
            let edited = edit_change(
                &squash.change,
                &repo.changes,
                &*txn.read(),
                &channel,
                &mut HashMap::default(),
                &preamble,
            )?;
            if hunks_text(&edited, &repo.changes)? != hunks_text(&squash.change, &repo.changes)? {
                bail!("The hunks of a squash cannot be edited")
            }
            squash.change.hashed.header = edited.hashed.header;
        }
        if squash.change.hashed.header.message.trim().is_empty() {
            bail!("Empty message, squash aborted")
        }

        let hash = sign_and_save(&mut squash.change, &secret, &repo.changes).await?;
        if let Err(e) =
            pijul_core::squash::apply_squash(&txn, &channel, &repo.changes, &squash, &hash)
        {
            repo.changes.del_change(&hash)?;
            return Err(e.into());
        }
        txn.commit()?;
        writeln!(std::io::stdout(), "Hash: {}", hash.to_base32())?;
        Ok(())
    }
}

/// The text of `change` without its header.
fn hunks_text<C: ChangeStore>(
    change: &pijul_core::change::Change,
    changes: &C,
) -> Result<Vec<u8>, anyhow::Error>
where
    C::Error: Send + Sync + 'static,
{
    let mut o = Vec::new();
    change.write(changes, None, false, &mut o)?;
    Ok(o)
}
