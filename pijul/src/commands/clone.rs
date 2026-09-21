use std::path::{Path, PathBuf};

use anyhow::bail;
use clap::{Parser, ValueHint};
use log::debug;
use pijul_core::{ChannelMutTxnT, MutTxnT};
use pijul_repository::*;

#[derive(Parser, Debug)]
pub struct Clone {
    /// Set the remote channel
    #[clap(long = "channel", default_value = pijul_core::DEFAULT_CHANNEL)]
    channel: String,
    /// Clone this change and its dependencies
    #[clap(long = "change", conflicts_with = "state")]
    change: Option<String>,
    /// Clone this state
    #[clap(long = "state", conflicts_with = "change")]
    state: Option<String>,
    /// Clone this path only
    #[clap(long = "path")]
    partial_paths: Vec<String>,
    /// Clone into a subdirectory of the *current* repository (monorepo mode):
    /// the remote channel is imported into the current channel and its
    /// sub-root is relocated under this directory. Must be a single-level
    /// directory name.
    #[clap(long = "into", value_hint = ValueHint::DirPath)]
    into: Option<PathBuf>,
    /// Do not check certificates (HTTPS remotes only, this option might be dangerous)
    #[clap(short = 'k')]
    no_cert_check: bool,
    /// Clone this remote
    remote: String,
    /// Path where to clone the repository.
    /// If missing, the inferred name of the remote repository is used.
    #[clap(value_hint = ValueHint::DirPath)]
    path: Option<PathBuf>,

    salt: Option<u64>,
}

impl Clone {
    pub fn repository_path(&self) -> Option<&Path> {
        None
    }

    pub async fn run(self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        if let Some(into) = self.into.clone() {
            return self.run_into(config, into).await;
        }
        let mut remote = pijul_remote::unknown_remote(
            config,
            None,
            None,
            &self.remote,
            &self.channel,
            self.no_cert_check,
            true,
        )
        .await?;

        let path = if let Some(path) = self.path {
            if path.is_relative() {
                let mut p = std::env::current_dir()?;
                p.push(path);
                p
            } else {
                path
            }
        } else if let Some(path) = remote.repo_name()? {
            let mut p = std::env::current_dir()?;
            p.push(path);
            p
        } else {
            bail!("Could not infer repository name from {:?}", self.remote)
        };
        debug!("path = {:?}", path);

        if std::fs::metadata(&path).is_ok() {
            bail!("Path {:?} already exists", path)
        }

        let repo_path = RepoPath::new(path.clone());
        let repo_path_ = repo_path.clone();
        ctrlc::set_handler(move || {
            repo_path_.remove();
            std::process::exit(130)
        })
        .unwrap_or(());

        let remote_normalised: std::borrow::Cow<str> = match remote {
            pijul_remote::RemoteRepo::Local(_) => std::fs::canonicalize(&self.remote)?
                .to_str()
                .unwrap()
                .to_string()
                .into(),
            _ => self.remote.as_str().into(),
        };
        let mut repo = Repository::init(config, Some(&path), None, Some(&remote_normalised))?;
        let txn = repo.pristine.arc_txn_begin()?;
        let channel_sm: pijul_core::small_string::SmallString = self.channel.parse()?;
        let mut channel = txn.write().open_or_create_channel(&channel_sm)?;
        if let Some(ref change) = self.change {
            let h = change.parse()?;
            remote
                .clone_tag(&mut repo, &txn, &mut channel, &[h])
                .await?
        } else if let Some(ref state) = self.state {
            let h = state.parse()?;
            remote.clone_state(&mut repo, &txn, &mut channel, h).await?
        } else {
            remote
                .clone_channel(&mut repo, &txn, &mut channel, &self.partial_paths)
                .await?;
        }

        if self.partial_paths.is_empty() {
            pijul_core::output::output_repository_no_pending_current(
                &repo.working_copy,
                &repo.changes,
                &txn,
                &channel,
                "",
                true,
                None,
                std::thread::available_parallelism()?.get(),
                self.salt.unwrap_or(0),
                true,
            )?;
        } else {
            for p in self.partial_paths.iter() {
                pijul_core::output::output_repository_no_pending_current(
                    &repo.working_copy,
                    &repo.changes,
                    &txn,
                    &channel,
                    p,
                    true,
                    None,
                    std::thread::available_parallelism()?.get(),
                    self.salt.unwrap_or(0),
                    true,
                )?;
            }
        }
        remote.finish().await?;
        txn.write().set_current_channel(&self.channel)?;

        let time = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u64;
        txn.write()
            .touch_channel(&mut *channel.write(), Some(time * 1000 + 1));

        txn.commit()?;
        std::mem::forget(repo_path);
        Ok(())
    }

    /// Monorepo clone: import the remote channel into the *current* repository's
    /// current channel, then relocate the imported sub-root under `into/`.
    async fn run_into(
        self,
        config: &pijul_config::Config,
        into: PathBuf,
    ) -> Result<(), anyhow::Error> {
        use pijul_core::changestore::ChangeStore;
        use pijul_core::pristine::{
            ChangeId, ChannelTxnT, EdgeFlags, GraphTxnT, Position, TxnT, Vertex, iter_adjacent,
        };

        // v1: a single-level destination directory.
        let dir_name = into
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid --into path {:?}", into))?;
        if dir_name.is_empty()
            || dir_name.contains('/')
            || dir_name.contains(std::path::MAIN_SEPARATOR)
        {
            bail!(
                "`--into` currently supports a single-level directory name (got {:?})",
                dir_name
            );
        }

        let mut repo = Repository::find_root(None)?;
        let txn = repo.pristine.arc_txn_begin()?;

        let channel_name = txn
            .read()
            .current_channel()
            .map(|c| c.to_string())
            .unwrap_or_else(|_| pijul_core::DEFAULT_CHANNEL.to_string());
        let channel_sm: pijul_core::small_string::SmallString = channel_name.parse()?;
        let mut channel = txn.write().open_or_create_channel(&channel_sm)?;

        // Alive FOLDER|BLOCK edges only.
        let f0 = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
        let f1 = f0 | EdgeFlags::PSEUDO;

        // Snapshot the existing sub-root NAME vertices (empty FOLDER children of
        // ROOT), and pick a destination parent: the INODE of an existing
        // sub-root, under which `<dir>` will be created.
        let (pre_names, dest_parent): (
            std::collections::HashSet<Vertex<ChangeId>>,
            Position<ChangeId>,
        ) = {
            let t = txn.read();
            let ch = channel.read();
            let graph = t.graph(&*ch);
            let mut names = std::collections::HashSet::new();
            let mut dest_parent = None;
            for e in iter_adjacent(&*t, graph, Vertex::ROOT, f0, f1)? {
                let e = e?;
                let child = *t.find_block(graph, e.dest()).unwrap();
                if child.start != child.end {
                    continue;
                }
                names.insert(child);
                if dest_parent.is_none() {
                    // The sub-root's INODE is the NAME's alive FOLDER child.
                    if let Some(e2) = iter_adjacent(&*t, graph, child, f0, f1)?.next() {
                        let e2 = e2?;
                        let inode = *t.find_block(graph, e2.dest()).unwrap();
                        dest_parent = Some(Position {
                            change: inode.change,
                            pos: inode.start,
                        });
                    }
                }
            }
            let dest_parent = dest_parent.ok_or_else(|| {
                anyhow::anyhow!(
                    "`clone --into` requires the current repository to already contain at \
                     least one sub-root; cloning into an empty repository is not yet supported"
                )
            })?;
            (names, dest_parent)
        };

        // Import the remote channel into the current channel.
        let mut remote = pijul_remote::unknown_remote(
            config,
            Some(&repo.path),
            None,
            &self.remote,
            &self.channel,
            self.no_cert_check,
            true,
        )
        .await?;
        remote
            .clone_channel(&mut repo, &txn, &mut channel, &self.partial_paths)
            .await?;

        // Identify the newly imported sub-root: a fresh empty FOLDER child of ROOT.
        let new_name = {
            let t = txn.read();
            let ch = channel.read();
            let graph = t.graph(&*ch);
            let mut found = None;
            for e in iter_adjacent(&*t, graph, Vertex::ROOT, f0, f1)? {
                let e = e?;
                let child = *t.find_block(graph, e.dest()).unwrap();
                if child.start == child.end && !pre_names.contains(&child) {
                    found = Some(child);
                    break;
                }
            }
            found.ok_or_else(|| {
                anyhow::anyhow!(
                    "could not identify the imported sub-root (no new top-level root vertex appeared)"
                )
            })?
        };

        // Build, save and apply the relocation change.
        let header = pijul_core::change::ChangeHeader {
            message: format!("Relocate cloned sub-root under {}/", dir_name),
            authors: vec![],
            description: None,
            timestamp: jiff::Timestamp::now(),
        };
        let mut reloc = pijul_core::record::relocate_sub_root(
            &*txn.read(),
            &channel,
            dest_parent,
            new_name,
            dir_name,
            header,
        )
        .map_err(|e| anyhow::anyhow!("relocate_sub_root: {:?}", e))?;
        let rh = repo
            .changes
            .save_change(&mut reloc, |_, _| Ok::<_, anyhow::Error>(()))?;
        pijul_core::apply::apply_change(
            &repo.changes,
            &mut *txn.write(),
            &mut *channel.write(),
            &rh,
        )?;

        // Materialise the working copy (now with the imported project under `dir/`).
        pijul_core::output::output_repository_no_pending_current(
            &repo.working_copy,
            &repo.changes,
            &txn,
            &channel,
            "",
            true,
            None,
            std::thread::available_parallelism()?.get(),
            self.salt.unwrap_or(0),
            true,
        )?;

        // Register the imported sub-root as a shared monorepo boundary in the
        // tracked `pijul.toml`, so `record` will refuse boundary-crossing moves
        // for everyone on the team. Decision (b) is to fold this edit *into* the
        // relocation change (atomic history); that is deferred until the
        // `clone --into` tree/inode reconciliation lands, since it requires a
        // content hunk merged with the synthetic relocation hunks. For now the
        // boundary is written to the working copy and picked up by the next
        // `record` (or `pijul.toml` is committed alongside).
        let boundary_added = pijul_config::add_boundary_to_shared(&repo.path, dir_name)?;

        remote.finish().await?;

        let time = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u64;
        txn.write()
            .touch_channel(&mut *channel.write(), Some(time * 1000 + 1));
        txn.commit()?;

        eprintln!(
            "Cloned into {}/ (experimental). The imported project is now a relocated \n\
             sub-root; a nested sub-root passthrough has no working-copy inode bridge yet, \n\
             so the next `pijul record` may report spurious moves — use `--split-per-root` \n\
             or `--force` until the tree/inode reconciliation lands.",
            dir_name
        );
        if boundary_added {
            eprintln!(
                "Registered `{}` as a monorepo boundary in {} — record it to share it; \n\
                 `pijul record` will now refuse moves that cross it (unless `--force`).",
                dir_name,
                pijul_config::SHARED_CONFIG_FILE,
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct RepoPath {
    path: PathBuf,
    remove_dir: bool,
    remove_dot: bool,
}

impl RepoPath {
    fn new(path: PathBuf) -> Self {
        RepoPath {
            remove_dir: std::fs::metadata(&path).is_err(),
            remove_dot: std::fs::metadata(&path.join(pijul_core::DOT_DIR)).is_err(),
            path,
        }
    }
    fn remove(&self) {
        if self.remove_dir {
            std::fs::remove_dir_all(&self.path).unwrap_or(());
        } else if self.remove_dot {
            std::fs::remove_dir_all(&self.path.join(pijul_core::DOT_DIR)).unwrap_or(());
        }
    }
}

impl Drop for RepoPath {
    fn drop(&mut self) {
        self.remove()
    }
}
