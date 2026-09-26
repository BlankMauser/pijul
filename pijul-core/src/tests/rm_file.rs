use super::*;

use crate::working_copy::{WorkingCopy, WorkingCopyRead};

#[test]
fn remove_file() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let contents = b"a\nb\nc\nd\ne\nf\n";

    let repo_alice = working_copy::memory::Memory::new();
    let repo_bob = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo_alice.add_file("a/b/c/d", contents.to_vec());

    let env_alice = pristine::sanakirja::Pristine::new_anon()?;
    let txn_alice = env_alice.arc_txn_begin().unwrap();
    let env_bob = pristine::sanakirja::Pristine::new_anon()?;
    let txn_bob = env_bob.arc_txn_begin().unwrap();
    let channel_alice = txn_alice
        .write()
        .open_or_create_channel(&SmallString::from_str("alice"))
        .unwrap();

    txn_alice.write().add_file("a/b/c/d", 0).unwrap();
    let init_h = record_all(&repo_alice, &changes, &txn_alice, &channel_alice, "")?;

    // Bob clones
    let channel_bob = txn_bob
        .write()
        .open_or_create_channel(&SmallString::from_str("bob"))
        .unwrap();
    apply::apply_change_arc(&changes, &txn_bob, &channel_bob, &init_h).unwrap();
    output::output_repository_no_pending(
        &repo_bob,
        &changes,
        &txn_bob,
        &channel_bob,
        "",
        true,
        None,
        1,
        0,
    )?;

    // Bob removes a/b and records
    repo_bob.remove_path("a/b/c", true)?;
    debug!("repo_bob = {:?}", repo_bob.list_files());
    let bob_h = record_all(&repo_bob, &changes, &txn_bob, &channel_bob, "").unwrap();

    // Alice applies Bob's change
    apply::apply_change_arc(&changes, &txn_alice, &channel_alice, &bob_h)?;
    output::output_repository_no_pending(
        &repo_alice,
        &changes,
        &txn_alice,
        &channel_alice,
        "",
        true,
        None,
        1,
        0,
    )?;
    Ok(())
}

/// A partial working copy recorded with `ignore_missing` can still delete an
/// explicitly chosen path in the same change, without touching absent
/// siblings.
#[test]
fn record_deleted_path_with_partial_working_copy() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let full = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    full.add_file("a/x", b"x\n".to_vec());
    full.add_file("a/y", b"y\n".to_vec());
    full.add_file("z", b"z\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();
    for path in ["a/x", "a/y", "z"] {
        txn.write().add_file(path, 0).unwrap();
    }
    record_all(&full, &changes, &txn, &channel, "")?;

    // Only the edited file is present; `a/y` is outside the edit and `z` is
    // deleted explicitly.
    let partial = working_copy::memory::Memory::new();
    partial.add_file("a/x", b"x edited\n".to_vec());
    let mut builder = Builder::new();
    builder.ignore_missing = true;
    builder.record(
        txn.clone(),
        Algorithm::default(),
        false,
        &crate::DEFAULT_SEPARATOR,
        channel.clone(),
        &partial,
        &changes,
        "a/x",
        1,
    )?;
    builder.record_deleted_path(&txn, &channel, &partial, &changes, "z")?;
    assert!(matches!(
        builder.record_deleted_path(&txn, &channel, &partial, &changes, "missing"),
        Err(crate::record::RecordError::PathNotInRepo(_))
    ));
    let mut recorded = builder.finish();
    let updatables = recorded.take_updatables();
    let hash = {
        let mut txn = txn.write();
        let mut change = recorded.into_change(
            &*txn,
            &channel,
            crate::change::ChangeHeader {
                message: "edit and delete".to_string(),
                ..crate::change::ChangeHeader::default()
            },
        )?;
        let hash = changes.save_change(&mut change, |_, _| Ok::<_, anyhow::Error>(()))?;
        txn.apply_local_change(&channel, &change, &hash, &updatables)?;
        hash
    };
    let change = changes.get_change(&hash)?;
    assert_eq!(
        change
            .changes
            .iter()
            .filter(|hunk| matches!(hunk, crate::change::Hunk::FileDel { .. }))
            .count(),
        1
    );
    assert!(!txn.read().is_tracked("z")?);
    assert!(txn.read().is_tracked("a/y")?);

    let fresh = working_copy::memory::Memory::new();
    output::output_repository_no_pending(&fresh, &changes, &txn, &channel, "", true, None, 1, 0)?;
    let mut files = fresh.list_files();
    files.sort();
    assert_eq!(files, vec!["a", "a/x", "a/y"]);
    let mut bytes = Vec::new();
    fresh.read_file("a/x", &mut bytes)?;
    assert_eq!(bytes, b"x edited\n");
    Ok(())
}
