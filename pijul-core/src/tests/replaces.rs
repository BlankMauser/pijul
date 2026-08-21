//! An amended change (one whose lineage names a `replaces` predecessor) must
//! supersede that predecessor when applied to a channel that has it, instead of
//! stacking on top (which surfaces as a conflict). This is what
//! `MutTxnTExt::unrecord_superseded` guarantees, and what `pijul pull`/`apply`
//! call before applying each incoming change.

use super::*;
use crate::MutTxnTExt;

fn on_channel<T: TxnT + GraphTxnT + ChannelTxnT + DepsTxnT>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    hash: &Hash,
) -> bool {
    let t = txn.read();
    match t.get_internal(&hash.into()).unwrap() {
        Some(internal) => t
            .get_changeset(t.changes(&*channel.read()), internal)
            .unwrap()
            .is_some(),
        None => false,
    }
}

#[test]
fn amend_supersedes_on_apply() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;

    // base + a first change (h1) on alice.
    repo.add_file("base.txt", b"base\n".to_vec());
    txn.write().add_file("base.txt", 0)?;
    let h_base = record_all(&repo, &changes, &txn, &alice, "")?;

    repo.add_file("feat.txt", b"feature\n".to_vec());
    txn.write().add_file("feat.txt", 0)?;
    let h1 = record_all(&repo, &changes, &txn, &alice, "")?;

    // "bob": a second channel that receives base + h1.
    let bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change_arc(&changes, &txn, &bob, &h_base)?;
    apply::apply_change_arc(&changes, &txn, &bob, &h1)?;
    assert!(
        on_channel(&txn, &bob, &h1),
        "bob should have h1 before the amend"
    );

    // Amend h1: record a fresh change and stamp its lineage `replaces = h1`
    // (this is what `pijul record --amend` writes into the change metadata).
    repo.add_file("fixed.txt", b"feature fixed\n".to_vec());
    txn.write().add_file("fixed.txt", 0)?;
    let (_unstamped, mut amend) = record_all_change(&repo, &changes, &txn, &alice, "")?;
    amend.hashed.set_change_metadata(Some(h1), Some(h1));
    let h1p = changes.save_change(&mut amend, |_, _| Ok::<_, anyhow::Error>(()))?;
    assert_eq!(
        amend.hashed.replaces(),
        Some(h1),
        "amend must declare replaces = h1"
    );

    // The fix: superseding before apply unrecords h1, so the amend replaces it
    // instead of stacking. Before this existed, `pull` skipped this step and
    // both changes coexisted as an order conflict.
    txn.write().unrecord_superseded(&changes, &bob, &h1p)?;
    assert!(
        !on_channel(&txn, &bob, &h1),
        "h1 must be superseded before applying the amend"
    );
    assert!(
        txn.read().is_superseded(&h1).unwrap(),
        "unrecord_superseded must record the obsolescence marker h1 -> h1p"
    );
    apply::apply_change_arc(&changes, &txn, &bob, &h1p)?;

    assert!(
        on_channel(&txn, &bob, &h1p),
        "bob should hold the amended change"
    );
    assert!(
        !on_channel(&txn, &bob, &h1),
        "bob must not keep the superseded change"
    );

    // No-op safety: superseding on a channel that never had h1 must not error.
    let carol = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("carol"))?;
    apply::apply_change_arc(&changes, &txn, &carol, &h_base)?;
    txn.write().unrecord_superseded(&changes, &carol, &h1p)?;

    Ok(())
}

// The reverse guarantee the `pull` filter relies on: once a local amend has
// superseded h1, the obsolescence marker survives a *superseding* unrecord (as
// used by the amend itself and by `unrecord_superseded`) but is cleared by a
// plain, "undo" unrecord — which is what lets `pull` skip a superseded
// predecessor yet still resurrect it if the amend is undone.
#[test]
fn superseded_marker_cleared_only_on_undo() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();

    let alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    repo.add_file("base.txt", b"base\n".to_vec());
    txn.write().add_file("base.txt", 0)?;
    let h_base = record_all(&repo, &changes, &txn, &alice, "")?;
    repo.add_file("feat.txt", b"feature\n".to_vec());
    txn.write().add_file("feat.txt", 0)?;
    let h1 = record_all(&repo, &changes, &txn, &alice, "")?;

    // Amend h1 -> h1p (stamp `replaces = h1`, as `record --amend` does).
    repo.add_file("fixed.txt", b"feature fixed\n".to_vec());
    txn.write().add_file("fixed.txt", 0)?;
    let (_unstamped, mut amend) = record_all_change(&repo, &changes, &txn, &alice, "")?;
    amend.hashed.set_change_metadata(Some(h1), Some(h1));
    let h1p = changes.save_change(&mut amend, |_, _| Ok::<_, anyhow::Error>(()))?;

    // A channel holding the amend: base + h1, then supersede h1 and apply h1p.
    let c = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("c"))?;
    apply::apply_change_arc(&changes, &txn, &c, &h_base)?;
    apply::apply_change_arc(&changes, &txn, &c, &h1)?;
    txn.write().unrecord_superseded(&changes, &c, &h1p)?;
    apply::apply_change_arc(&changes, &txn, &c, &h1p)?;
    assert!(
        txn.read().is_superseded(&h1).unwrap(),
        "marker present once the amend is applied"
    );

    // A superseding unrecord (amend / unrecord_superseded) keeps the marker.
    {
        let mut touched = crate::unrecord::TouchedInodes::default();
        txn.write()
            .unrecord_superseding(&changes, &c, &h1p, 0, &mut touched)?;
    }
    assert!(
        txn.read().is_superseded(&h1).unwrap(),
        "a superseding unrecord must NOT clear the marker"
    );

    // A plain (undo) unrecord resurrects h1 by clearing the marker.
    apply::apply_change_arc(&changes, &txn, &c, &h1p)?;
    {
        let mut touched = crate::unrecord::TouchedInodes::default();
        txn.write().unrecord(&changes, &c, &h1p, 0, &mut touched)?;
    }
    assert!(
        !txn.read().is_superseded(&h1).unwrap(),
        "an undo unrecord must clear the marker"
    );

    Ok(())
}
