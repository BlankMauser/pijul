//! An amended change (one whose lineage carries a `replaces` chain) must
//! supersede that chain when applied to a channel that has any of it, instead of
//! stacking on top (which surfaces as a conflict). This is what
//! `MutTxnTExt::unrecord_superseded` guarantees, and what `pijul pull`/`apply`
//! call before applying each incoming change. Because the *whole* chain travels
//! in the metadata, a peer that never saw the intermediate iterations still
//! supersedes every one of them (see `chained_amend_supersedes_whole_chain`).

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
    amend.hashed.set_change_metadata(&[h1]);
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

// Marker lifetime under the uniform (single) unrecord path. Re-amending keeps
// the *whole* prior chain superseded without any "superseding" unrecord variant:
// the re-amend unrecords its predecessor (dropping that predecessor's markers)
// and its own `unrecord_superseded` re-marks the entire chain. Undoing the tip
// with a plain unrecord — the only kind now — drops the markers it owns, so the
// resurrected predecessors are no longer filtered by `pull`. This is the
// property the old `superseded_marker_cleared_only_on_undo` test guarded, now
// expressed against a *chain* rather than a preserved/cleared single marker.
#[test]
fn reamend_supersedes_whole_chain_then_undo_resurrects() -> Result<(), anyhow::Error> {
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

    // Amend h1 -> h1p (chain [h1]).
    repo.add_file("fix1.txt", b"fix one\n".to_vec());
    txn.write().add_file("fix1.txt", 0)?;
    let (_u1, mut amend1) = record_all_change(&repo, &changes, &txn, &alice, "")?;
    amend1.hashed.set_change_metadata(&[h1]);
    let h1p = changes.save_change(&mut amend1, |_, _| Ok::<_, anyhow::Error>(()))?;

    // Re-amend h1p -> h1pp (chain [h1, h1p]), as `record --amend` builds it:
    // predecessor's chain, extended with the predecessor.
    repo.add_file("fix2.txt", b"fix two\n".to_vec());
    txn.write().add_file("fix2.txt", 0)?;
    let (_u2, mut amend2) = record_all_change(&repo, &changes, &txn, &alice, "")?;
    let mut chain = changes
        .get_change(&h1p)
        .ok()
        .map(|c| c.replaces_chain())
        .unwrap_or_default();
    chain.push(h1p);
    amend2.hashed.set_change_metadata(&chain);
    let h1pp = changes.save_change(&mut amend2, |_, _| Ok::<_, anyhow::Error>(()))?;
    assert_eq!(amend2.hashed.replaces_chain(), vec![h1, h1p]);

    // Channel c: base + h1, then apply the first amend h1p.
    let c = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("c"))?;
    apply::apply_change_arc(&changes, &txn, &c, &h_base)?;
    apply::apply_change_arc(&changes, &txn, &c, &h1)?;
    txn.write().unrecord_superseded(&changes, &c, &h1p)?;
    apply::apply_change_arc(&changes, &txn, &c, &h1p)?;
    assert!(
        txn.read().is_superseded(&h1).unwrap(),
        "h1 superseded by h1p"
    );

    // Apply the re-amend h1pp: it supersedes the whole chain [h1, h1p].
    txn.write().unrecord_superseded(&changes, &c, &h1pp)?;
    apply::apply_change_arc(&changes, &txn, &c, &h1pp)?;
    assert!(
        txn.read().is_superseded(&h1).unwrap(),
        "re-amend keeps h1 superseded"
    );
    assert!(
        txn.read().is_superseded(&h1p).unwrap(),
        "re-amend supersedes the intermediate h1p too"
    );
    assert!(!on_channel(&txn, &c, &h1) && !on_channel(&txn, &c, &h1p));
    assert!(
        on_channel(&txn, &c, &h1pp),
        "only the tip is on the channel"
    );

    // Undo the tip with a plain unrecord (the only kind): h1pp leaves every
    // channel, so it drops the markers it owns and resurrects the whole chain.
    {
        let mut touched = crate::unrecord::TouchedInodes::default();
        txn.write().unrecord(&changes, &c, &h1pp, 0, &mut touched)?;
    }
    assert!(
        !txn.read().is_superseded(&h1).unwrap(),
        "undoing the tip clears h1's marker"
    );
    assert!(
        !txn.read().is_superseded(&h1p).unwrap(),
        "undoing the tip clears h1p's marker"
    );

    Ok(())
}

// A *chain* of amends recorded before the first push, pushed to a server that
// knows only the root — the case the one-hop `replaces` pointer could not
// handle. Alice amends h1 twice locally — h1 -> a1 -> a2 — so the tip a2 carries
// the full chain `replaces = [h1, a1]` and `root = h1`. She pushes only the tip
// a2 to a server that has just the root h1; the intermediate a1 never travels.
// Because a2 carries the whole chain, `unrecord_superseded(a2)` marks *every*
// element (incl. h1) and unrecords h1 from the channel — so a2 cleanly replaces
// the root instead of stacking on it, with a single author and no concurrency.
#[test]
fn chained_amend_supersedes_whole_chain() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();

    let alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;

    // base + original change h1 on alice.
    repo.add_file("base.txt", b"base\n".to_vec());
    txn.write().add_file("base.txt", 0)?;
    let h_base = record_all(&repo, &changes, &txn, &alice, "")?;
    repo.add_file("feat.txt", b"feature\n".to_vec());
    txn.write().add_file("feat.txt", 0)?;
    let h1 = record_all(&repo, &changes, &txn, &alice, "")?;

    // First amend h1 -> a1 (chain [h1]).
    repo.add_file("fix1.txt", b"fix one\n".to_vec());
    txn.write().add_file("fix1.txt", 0)?;
    let (_unstamped1, mut a1) = record_all_change(&repo, &changes, &txn, &alice, "")?;
    a1.hashed.set_change_metadata(&[h1]);
    let h_a1 = changes.save_change(&mut a1, |_, _| Ok::<_, anyhow::Error>(()))?;
    assert_eq!(a1.hashed.replaces_chain(), vec![h1]);
    assert_eq!(a1.hashed.root(), Some(h1));

    // Second amend a1 -> a2, built exactly as `record --amend` does: the
    // predecessor's chain, extended with the predecessor.
    repo.add_file("fix2.txt", b"fix two\n".to_vec());
    txn.write().add_file("fix2.txt", 0)?;
    let (_unstamped2, mut a2) = record_all_change(&repo, &changes, &txn, &alice, "")?;
    let mut chain = changes
        .get_change(&h_a1)
        .ok()
        .map(|c| c.replaces_chain())
        .unwrap_or_default();
    chain.push(h_a1);
    a2.hashed.set_change_metadata(&chain);
    let h_a2 = changes.save_change(&mut a2, |_, _| Ok::<_, anyhow::Error>(()))?;
    assert_eq!(
        a2.hashed.replaces_chain(),
        vec![h1, h_a1],
        "tip carries the whole chain: root h1, then intermediate a1"
    );
    assert_eq!(
        a2.hashed.replaces(),
        Some(h_a1),
        "immediate predecessor = a1"
    );
    assert_eq!(a2.hashed.root(), Some(h1), "root of the lineage = h1");

    // The server knows only base + the root h1. The intermediate a1 never
    // reached it.
    let server = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("server"))?;
    apply::apply_change_arc(&changes, &txn, &server, &h_base)?;
    apply::apply_change_arc(&changes, &txn, &server, &h1)?;
    assert!(on_channel(&txn, &server, &h1), "server has the root h1");

    // Push the tip a2: supersede-before-apply (as pull/apply do), then apply.
    txn.write().unrecord_superseded(&changes, &server, &h_a2)?;
    apply::apply_change_arc(&changes, &txn, &server, &h_a2)?;

    // The whole chain is marked superseded — including the root, which the
    // one-hop design could never reach.
    assert!(
        txn.read().is_superseded(&h1).unwrap(),
        "the root h1 is superseded — a2 carries it in its chain"
    );
    assert!(
        txn.read().is_superseded(&h_a1).unwrap(),
        "the intermediate a1 is superseded too"
    );
    // The root left the channel; only the amend remains.
    assert!(
        !on_channel(&txn, &server, &h1),
        "h1 was unrecorded — a2 replaced it instead of stacking"
    );
    assert!(
        on_channel(&txn, &server, &h_a2),
        "the amend is on the channel"
    );

    Ok(())
}
