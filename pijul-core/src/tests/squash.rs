use super::*;
use crate::change::{ChangeHeader, Hunk};
use crate::squash::SquashError;
use crate::working_copy::{WorkingCopy, WorkingCopyRead};
use std::io::Write;

type Store = changestore::memory::Memory;

/// Squash `hashes`, save and apply the result, as a caller would.
fn squash_all<T>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    changes: &Store,
    hashes: &[Hash],
) -> Result<Hash, anyhow::Error>
where
    T: MutTxnT + Send + Sync + 'static,
    T::Channel: Send + Sync,
{
    squash_with(txn, channel, changes, hashes, false)
}

pub(super) fn squash_with<T>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    changes: &Store,
    hashes: &[Hash],
    force: bool,
) -> Result<Hash, anyhow::Error>
where
    T: MutTxnT + Send + Sync + 'static,
    T::Channel: Send + Sync,
{
    let header = ChangeHeader {
        message: "squash".to_string(),
        ..Default::default()
    };
    let mut s = crate::squash::squash(txn, channel, changes, hashes, header, force)?;
    let hash = changes.save_change(&mut s.change, |_, _| Ok::<_, anyhow::Error>(()))?;
    crate::squash::apply_squash(txn, channel, changes, &s, &hash)?;
    Ok(hash)
}

pub(super) fn on_channel<T: ChannelTxnT>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    h: &Hash,
) -> bool {
    let txn = txn.read();
    match txn.get_internal(&h.into()).unwrap() {
        Some(id) => txn
            .get_changeset(txn.changes(&*channel.read()), id)
            .unwrap()
            .is_some(),
        None => false,
    }
}

/// Hunks a record of `repo` would produce right now, without applying them.
pub(super) fn pending<T>(
    repo: &working_copy::memory::Memory,
    changes: &Store,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
) -> usize
where
    T: MutTxnT + Send + Sync + 'static,
    T::Channel: Send + Sync,
{
    let mut state = Builder::new();
    state
        .record(
            txn.clone(),
            Algorithm::default(),
            false,
            &crate::DEFAULT_SEPARATOR,
            channel.clone(),
            repo,
            changes,
            "",
            1,
        )
        .unwrap();
    state.finish().actions.len()
}

/// Contents of `path` in `channel`, read from a scratch output so that the
/// real tree is left alone.
fn channel_file<T>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    changes: &Store,
    path: &str,
) -> Option<Vec<u8>>
where
    T: MutTxnT + Send + Sync + 'static,
    T::Channel: Send + Sync,
{
    txn.write().begin_scratch_tree().unwrap();
    let view = working_copy::memory::Memory::new();
    let conflicts =
        output::output_repository_no_pending_(&view, changes, txn, channel, "", true, None, 1, 0)
            .unwrap();
    assert!(conflicts.is_empty());
    txn.write().end_scratch_tree().unwrap();
    let mut buf = Vec::new();
    view.read_file(path, &mut buf).ok().map(|_| buf)
}

#[test]
fn squash_edits_keeps_pending() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let changes = Store::new();
    repo.add_file("dir/file", b"a\nb\nc\n".to_vec());
    repo.add_file("dir/other", b"o\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("dir/file", 0)?;
    txn.write().add_file("dir/other", 0)?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;

    repo.write_file("dir/file", Inode::ROOT)?
        .write_all(b"a\nx\nc\n")?;
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    repo.write_file("dir/file", Inode::ROOT)?
        .write_all(b"a\nx\ny\nc\n")?;
    let h2 = record_all(&repo, &changes, &txn, &channel, "")?;

    // Unrecorded edit, which the squash must not swallow.
    repo.write_file("dir/file", Inode::ROOT)?
        .write_all(b"a\nx\ny\nc\npending\n")?;

    let s = squash_all(&txn, &channel, &changes, &[h2, h1])?;

    assert!(on_channel(&txn, &channel, &h0));
    assert!(on_channel(&txn, &channel, &s));
    assert!(!on_channel(&txn, &channel, &h1));
    assert!(!on_channel(&txn, &channel, &h2));
    assert!(txn.read().is_superseded(&h1)?);
    assert!(txn.read().is_superseded(&h2)?);

    let c = changes.get_change(&s)?;
    assert_eq!(c.replaces_chain(), vec![h1, h2]);
    assert!(!c.changes.is_empty());
    assert!(
        c.changes
            .iter()
            .all(|h| !matches!(h, Hunk::FileAdd { .. } | Hunk::FileDel { .. }))
    );

    assert_eq!(pending(&repo, &changes, &txn, &channel), 1);
    assert_eq!(
        channel_file(&txn, &channel, &changes, "dir/file").as_deref(),
        Some(&b"a\nx\ny\nc\n"[..])
    );
    assert_eq!(
        channel_file(&txn, &channel, &changes, "dir/other").as_deref(),
        Some(&b"o\n"[..])
    );
    txn.commit()?;
    Ok(())
}

#[test]
fn squash_add_then_edit() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let changes = Store::new();
    repo.add_file("dir/file", b"a\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("dir/file", 0)?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    repo.add_file("dir/new", b"1\n".to_vec());
    txn.write().add_file("dir/new", 0)?;
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    repo.write_file("dir/new", Inode::ROOT)?
        .write_all(b"1\n2\n")?;
    let h2 = record_all(&repo, &changes, &txn, &channel, "")?;

    let s = squash_all(&txn, &channel, &changes, &[h1, h2])?;
    let c = changes.get_change(&s)?;
    assert_eq!(
        c.changes
            .iter()
            .filter(|h| matches!(h, Hunk::FileAdd { .. }))
            .count(),
        1
    );

    // The real tree follows the file to its new inode vertex: nothing to record.
    assert_eq!(pending(&repo, &changes, &txn, &channel), 0);
    assert_eq!(
        channel_file(&txn, &channel, &changes, "dir/new").as_deref(),
        Some(&b"1\n2\n"[..])
    );

    // And recording on top of it still works.
    repo.write_file("dir/new", Inode::ROOT)?
        .write_all(b"1\n2\n3\n")?;
    record_all(&repo, &changes, &txn, &channel, "")?;
    assert_eq!(pending(&repo, &changes, &txn, &channel), 0);
    assert_eq!(
        channel_file(&txn, &channel, &changes, "dir/new").as_deref(),
        Some(&b"1\n2\n3\n"[..])
    );
    Ok(())
}

#[test]
fn squash_deletion() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let changes = Store::new();
    repo.add_file("dir/file", b"a\n".to_vec());
    repo.add_file("dir/other", b"o\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("dir/file", 0)?;
    txn.write().add_file("dir/other", 0)?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    repo.remove_path("dir/other", false)?;
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    repo.write_file("dir/file", Inode::ROOT)?
        .write_all(b"a\nb\n")?;
    let h2 = record_all(&repo, &changes, &txn, &channel, "")?;

    let s = squash_all(&txn, &channel, &changes, &[h1, h2])?;
    let c = changes.get_change(&s)?;
    assert!(c.changes.iter().any(|h| matches!(h, Hunk::FileDel { .. })));

    assert_eq!(pending(&repo, &changes, &txn, &channel), 0);
    assert_eq!(channel_file(&txn, &channel, &changes, "dir/other"), None);
    assert_eq!(
        channel_file(&txn, &channel, &changes, "dir/file").as_deref(),
        Some(&b"a\nb\n"[..])
    );
    Ok(())
}

/// A change that is not squashed sits between the squashed ones, on the same
/// file.
#[test]
fn squash_around_independent_change() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let changes = Store::new();
    repo.add_file("file", b"a\nb\nc\nd\ne\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("file", 0)?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nB\nc\nd\ne\n")?;
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nB\nc\nd\nE\n")?;
    let hq = record_all(&repo, &changes, &txn, &channel, "")?;
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nBB\nc\nd\nE\n")?;
    let h2 = record_all(&repo, &changes, &txn, &channel, "")?;

    let s = squash_all(&txn, &channel, &changes, &[h1, h2])?;
    assert!(on_channel(&txn, &channel, &hq));
    assert!(on_channel(&txn, &channel, &s));
    assert!(!changes.get_change(&s)?.dependencies.contains(&hq));
    assert_eq!(pending(&repo, &changes, &txn, &channel), 0);
    assert_eq!(
        channel_file(&txn, &channel, &changes, "file").as_deref(),
        Some(&b"a\nBB\nc\nd\nE\n"[..])
    );
    Ok(())
}

/// Squashing a change together with the resolution of its conflict is fine.
#[test]
fn squash_with_resolution() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo_alice = working_copy::memory::Memory::new();
    let repo_bob = working_copy::memory::Memory::new();
    let changes = Store::new();
    repo_alice.add_file("file", b"a\nb\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let h0 = record_all(&repo_alice, &changes, &txn, &alice, "")?;

    let bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change(&changes, &mut *txn.write(), &mut *bob.write(), &h0)?;
    output::output_repository_no_pending(&repo_bob, &changes, &txn, &bob, "", true, None, 1, 0)?;
    repo_bob
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\ny\nb\n")?;
    let hb = record_all(&repo_bob, &changes, &txn, &bob, "")?;

    repo_alice
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nx\nb\n")?;
    let ha = record_all(&repo_alice, &changes, &txn, &alice, "")?;
    apply::apply_change(&changes, &mut *txn.write(), &mut *alice.write(), &hb)?;
    let conflicts = output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn,
        &alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    assert!(!conflicts.is_empty());
    repo_alice
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nx\ny\nb\n")?;
    let hr = record_all(&repo_alice, &changes, &txn, &alice, "")?;

    let s = squash_all(&txn, &alice, &changes, &[ha, hr])?;
    assert!(on_channel(&txn, &alice, &hb));
    assert!(changes.get_change(&s)?.dependencies.contains(&hb));
    assert_eq!(pending(&repo_alice, &changes, &txn, &alice), 0);
    assert_eq!(
        channel_file(&txn, &alice, &changes, "file").as_deref(),
        Some(&b"a\nx\ny\nb\n"[..])
    );
    Ok(())
}

/// A repository without a working copy (as on a server) can squash too.
#[test]
fn squash_without_working_copy() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let changes = Store::new();
    repo.add_file("dir/file", b"a\n".to_vec());
    repo.add_file("dir/other", b"o\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("dir/file", 0)?;
    txn.write().add_file("dir/other", 0)?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;
    repo.add_file("dir/new", b"n\n".to_vec());
    txn.write().add_file("dir/new", 0)?;
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    repo.write_file("dir/file", Inode::ROOT)?
        .write_all(b"a\nb\n")?;
    repo.remove_path("dir/other", false)?;
    let h2 = record_all(&repo, &changes, &txn, &channel, "")?;

    let server_env = pristine::sanakirja::Pristine::new_anon()?;
    let server = server_env.arc_txn_begin().unwrap();
    let main = server
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    for h in [h0, h1, h2] {
        apply::apply_change_arc(&changes, &server, &main, &h)?;
    }
    assert!(server.read().iter_inodes()?.next().is_none());

    let s = squash_all(&server, &main, &changes, &[h1, h2])?;
    assert!(on_channel(&server, &main, &s));
    assert!(server.read().iter_inodes()?.next().is_none());
    assert_eq!(
        channel_file(&server, &main, &changes, "dir/new").as_deref(),
        Some(&b"n\n"[..])
    );
    assert_eq!(
        channel_file(&server, &main, &changes, "dir/file").as_deref(),
        Some(&b"a\nb\n"[..])
    );
    assert_eq!(channel_file(&server, &main, &changes, "dir/other"), None);
    server.commit()?;
    Ok(())
}

/// Alice records h0, h1, h2 and pushes them to Bob (another pristine), then
/// squashes h1 and h2 into a new change.
#[allow(clippy::type_complexity)]
fn squashed_after_push() -> Result<(Store, pristine::sanakirja::Pristine, [Hash; 4]), anyhow::Error>
{
    let repo = working_copy::memory::Memory::new();
    let changes = Store::new();
    repo.add_file("file", b"a\nb\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("file", 0)?;
    let h0 = record_all(&repo, &changes, &txn, &alice, "")?;
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nX\nb\n")?;
    let h1 = record_all(&repo, &changes, &txn, &alice, "")?;
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nX\nY\nb\n")?;
    let h2 = record_all(&repo, &changes, &txn, &alice, "")?;

    let bob_env = pristine::sanakirja::Pristine::new_anon()?;
    {
        let bob_txn = bob_env.arc_txn_begin().unwrap();
        let bob = bob_txn
            .write()
            .open_or_create_channel(&SmallString::from_str("main"))?;
        for h in [h0, h1, h2] {
            apply::apply_change_arc(&changes, &bob_txn, &bob, &h)?;
        }
        bob_txn.commit()?;
    }

    let s = squash_all(&txn, &alice, &changes, &[h1, h2])?;
    Ok((changes, bob_env, [h0, h1, h2, s]))
}

#[test]
fn squash_applies_on_peer() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let (changes, bob_env, [h0, h1, h2, s]) = squashed_after_push()?;

    let txn = bob_env.arc_txn_begin().unwrap();
    let bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().unrecord_superseded(&changes, &bob, &s)?;
    apply::apply_change_arc(&changes, &txn, &bob, &s)?;

    assert!(on_channel(&txn, &bob, &h0));
    assert!(on_channel(&txn, &bob, &s));
    assert!(!on_channel(&txn, &bob, &h1));
    assert!(!on_channel(&txn, &bob, &h2));
    assert_eq!(
        channel_file(&txn, &bob, &changes, "file").as_deref(),
        Some(&b"a\nX\nY\nb\n"[..])
    );
    Ok(())
}

/// Bob built on top of the squashed changes: applying the squash must fail
/// before unrecording anything.
#[test]
fn squash_refused_on_peer_with_dependent() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let (changes, bob_env, [_, h1, h2, s]) = squashed_after_push()?;

    let repo = working_copy::memory::Memory::new();
    let txn = bob_env.arc_txn_begin().unwrap();
    let bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    output::output_repository_no_pending(&repo, &changes, &txn, &bob, "", true, None, 1, 0)?;
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nX\nZ\nY\nb\n")?;
    let hq = record_all(&repo, &changes, &txn, &bob, "")?;

    match txn.write().unrecord_superseded(&changes, &bob, &s) {
        Err(crate::unrecord::UnrecordError::ChangeIsDependedUpon { .. }) => {}
        Err(e) => panic!("unexpected error {:?}", e),
        Ok(()) => panic!("applying the squash should have been refused"),
    }
    assert!(on_channel(&txn, &bob, &h1));
    assert!(on_channel(&txn, &bob, &h2));
    assert!(on_channel(&txn, &bob, &hq));
    Ok(())
}

#[test]
fn squash_refuses_dependent() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let changes = Store::new();
    repo.add_file("file", b"a\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn.write().add_file("file", 0)?;
    record_all(&repo, &changes, &txn, &channel, "")?;
    repo.write_file("file", Inode::ROOT)?.write_all(b"a\nb\n")?;
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nc\n")?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    match crate::squash::squash(
        &txn,
        &channel,
        &changes,
        &[h1],
        ChangeHeader::default(),
        true,
    ) {
        Err(SquashError::DependedUpon { hash, .. }) => assert_eq!(hash, h1),
        Err(e) => panic!("unexpected error {:?}", e),
        Ok(_) => panic!("squash should have been refused"),
    }
    Ok(())
}

#[test]
fn squash_refuses_conflict() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo_alice = working_copy::memory::Memory::new();
    let repo_bob = working_copy::memory::Memory::new();
    let changes = Store::new();
    repo_alice.add_file("file", b"a\nb\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let alice = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))?;
    txn.write().add_file("file", 0)?;
    let h0 = record_all(&repo_alice, &changes, &txn, &alice, "")?;

    let bob = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))?;
    apply::apply_change(&changes, &mut *txn.write(), &mut *bob.write(), &h0)?;
    output::output_repository_no_pending(&repo_bob, &changes, &txn, &bob, "", true, None, 1, 0)?;
    repo_bob
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\ny\nb\n")?;
    let hb = record_all(&repo_bob, &changes, &txn, &bob, "")?;

    repo_alice
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nx\nb\n")?;
    let ha = record_all(&repo_alice, &changes, &txn, &alice, "")?;
    apply::apply_change(&changes, &mut *txn.write(), &mut *alice.write(), &hb)?;

    match crate::squash::squash(&txn, &alice, &changes, &[ha], ChangeHeader::default(), true) {
        Err(SquashError::Conflicts { conflicts }) => assert!(!conflicts.is_empty()),
        Err(e) => panic!("unexpected error {:?}", e),
        Ok(_) => panic!("squash should have been refused"),
    }
    Ok(())
}
