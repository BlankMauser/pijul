use crate::commands::common_opts::RepoAndChannel;
use crate::commands::load_channel;
use clap::Parser;
use log::*;
use pijul_core::TxnTExt;
use std::path::Path;

#[derive(Parser, Debug)]
pub struct Debug {
    #[clap(flatten)]
    base: RepoAndChannel,
    #[clap(long = "sanakirja-only")]
    sanakirja_only: bool,
    root: Option<String>,
}

impl Debug {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;
        let txn = repo.pristine.txn_begin()?;
        let (channel, _) = load_channel(self.base.channel(), &txn)?;
        if !self.sanakirja_only {
            pijul_core::pristine::debug_inodes(&txn);
            pijul_core::pristine::debug_dep(&txn);
            pijul_core::pristine::debug_revdep(&txn);
            pijul_core::pristine::debug_revinodes(&txn);
            pijul_core::pristine::debug_tree_print(&txn);
            pijul_core::pristine::debug_revtree_print(&txn);
            pijul_core::pristine::debug_remotes(&txn);
            if let Some(root) = self.root {
                let pos = if let Some(pos) = parse_pos(&root) {
                    pos
                } else {
                    let inode = pijul_core::fs::find_inode(&txn, &root)?;
                    debug!("inode {:?}", inode);
                    use pijul_core::TreeTxnT;
                    if let Ok(Some(pos)) = txn.get_inodes(&inode, None) {
                        debug!("inode {:?}", pos);
                        *pos
                    } else {
                        debug!("no inode");
                        txn.follow_oldest_path(&repo.changes, &channel, &root)?.0
                    }
                };

                pijul_core::pristine::debug_root(
                    &txn,
                    &channel.read(),
                    pos.inode_vertex(),
                    std::io::stdout(),
                    true,
                )?;
            } else {
                let channel = channel.read();
                pijul_core::pristine::debug(&txn, &channel, std::io::stdout())?;
            }
            pijul_core::pristine::check_alive_debug(&repo.changes, &txn, &*channel.read(), 0)?;
        }
        ::sanakirja::debug::debug(&txn.txn, &[&txn.tree], "debug.tree", true);
        eprintln!(
            "{:#?}",
            txn.check_database(&mut std::collections::BTreeMap::new())
        );
        let channel = channel.read();
        ::sanakirja::debug::debug(&txn.txn, &[&channel.graph], "debug.sanakirja", true);
        Ok(())
    }
}

fn parse_pos(s: &str) -> Option<pijul_core::pristine::Position<pijul_core::pristine::ChangeId>> {
    let mut it = s.split('.');
    if let (Some(a), Some(b)) = (it.next(), it.next()) {
        use pijul_core::pristine::{Base32, ChangeId, ChangePosition, Position};
        let change = ChangeId::from_base32(a.as_bytes())?;
        let pos: u64 = b.parse().ok()?;
        Some(Position {
            change,
            pos: ChangePosition(pos.into()),
        })
    } else {
        None
    }
}
