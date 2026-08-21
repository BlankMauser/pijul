//! `inodes` and `revinodes` must stay perfect inverses of each other.
//!
//! `inodes` is single-valued per inode (`put_inodes` replaces), while
//! `revinodes` is a plain multi-valued map keyed by position. Re-pointing an
//! inode at a new position used to leak the previous `revinodes[old] = inode`
//! edge; a later deletion then orphaned it, leaving `get_revinodes(old)`
//! pointing at an inode with no `get_inodes` entry — the exact state that made
//! `pijul reset` panic in `output/working_copy.rs`.

use super::*;
use crate::pristine::{
    ChangeId, ChangePosition, L64, Position, TreeTxnT, del_inodes_with_rev, put_inodes_with_rev,
};

fn pos(change: u64, p: u64) -> Position<ChangeId> {
    Position {
        change: ChangeId(L64(change)),
        pos: ChangePosition(L64(p)),
    }
}

/// After every mutation, no `revinodes` entry may point at an inode whose
/// `inodes` entry is missing or points elsewhere.
fn assert_symmetric<T: TreeTxnT>(txn: &T) {
    for x in txn.iter_revinodes().unwrap() {
        let (position, inode) = x.unwrap();
        match txn.get_inodes(inode, None).unwrap() {
            Some(p) => assert_eq!(
                p, position,
                "revinodes[{:?}] = {:?}, but inodes[{:?}] = {:?}",
                position, inode, inode, p
            ),
            None => panic!(
                "orphaned revinodes entry: revinodes[{:?}] = {:?}, inodes[{:?}] missing",
                position, inode, inode
            ),
        }
    }
}

#[test]
fn inode_repoint_keeps_symmetry() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let mut txn = env.mut_txn_begin()?;

    let inode = Inode(L64(42));
    let p1 = pos(1, 100);
    let p2 = pos(2, 200);

    // Bind the inode to p1, then re-point it to p2 (as `update_inode`'s
    // `InodeUpdate::Add` does when the same working-copy inode is recorded at a
    // new position).
    put_inodes_with_rev(&mut txn, &inode, &p1)?;
    assert_symmetric(&txn);
    put_inodes_with_rev(&mut txn, &inode, &p2)?;

    // The old reverse edge must be gone, not merely shadowed.
    assert_eq!(
        txn.get_revinodes(&p1, None)?,
        None,
        "stale revinodes[p1] leaked"
    );
    assert_eq!(txn.get_revinodes(&p2, None)?.map(|x| *x), Some(inode));
    assert_eq!(txn.get_inodes(&inode, None)?.map(|x| *x), Some(p2));
    assert_symmetric(&txn);

    // Deleting the inode must leave no orphaned reverse edge behind.
    del_inodes_with_rev(&mut txn, &inode, &p2)?;
    assert_eq!(txn.get_inodes(&inode, None)?, None);
    assert_eq!(txn.get_revinodes(&p1, None)?, None);
    assert_eq!(txn.get_revinodes(&p2, None)?, None);
    assert_symmetric(&txn);

    Ok(())
}
