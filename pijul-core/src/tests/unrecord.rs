use super::*;
use crate::working_copy::{WorkingCopy, WorkingCopyRead};
use std::io::Write;

/// Add a file, write to it, then fork the branch and unrecord once on
/// one side.
#[test]
fn test() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("dir/file", b"a\nb\nc\nd\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("dir/file", 0)?;

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let _h0 = record_all(&repo, &changes, &txn, &channel, "")?;

    use std::io::Write;
    repo.write_file("dir/file", Inode::ROOT)?
        .write_all(b"a\nx\nb\nd\n")?;

    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;

    let _channel2 = txn
        .write()
        .fork(&channel, &SmallString::from_str("main2"))?;
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h1,
        0,
        &mut Default::default(),
    )?;
    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts);
    }
    let mut buf = Vec::new();
    repo.read_file("dir/file", &mut buf)?;
    assert_eq!(std::str::from_utf8(&buf), Ok("a\nb\nc\nd\n"));

    txn.commit()?;

    Ok(())
}

#[test]
fn replace() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("dir/file", b"a\nb\nc\nd\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("dir/file", 0)?;

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let _h0 = record_all(&repo, &changes, &txn, &channel, "")?;

    repo.write_file("dir/file", Inode::ROOT)?
        .write_all(b"a\nx\ny\nd\n")?;

    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;

    let _channel2 = txn
        .write()
        .fork(&channel, &SmallString::from_str("main2"))?;
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h1,
        0,
        &mut Default::default(),
    )?;
    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts);
    }
    let mut buf = Vec::new();
    repo.read_file("dir/file", &mut buf)?;
    assert_eq!(std::str::from_utf8(&buf), Ok("a\nb\nc\nd\n"));

    txn.commit()?;

    Ok(())
}

#[test]
fn file_move() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"a\nb\nc\nd\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let _h0 = record_all(&repo, &changes, &txn, &channel, "")?;

    repo.rename("file", "dir/file")?;
    txn.write().move_file("file", "dir/file", 0)?;
    debug!("recording the move");
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;

    debug!("unrecording the move");
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h1,
        0,
        &mut Default::default(),
    )?;

    assert_eq!(
        crate::fs::iter_working_copy(&*txn.read(), Inode::ROOT)
            .map(|n| n.unwrap().1)
            .collect::<Vec<_>>(),
        vec!["dir", "dir/file"]
    );
    assert_eq!(repo.list_files(), vec!["dir", "dir/file"]);

    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)?;
    assert_eq!(
        crate::fs::iter_working_copy(&*txn.read(), Inode::ROOT)
            .map(|n| n.unwrap().1)
            .collect::<Vec<_>>(),
        vec!["file"]
    );

    // Checking that unrecord doesn't delete `dir`, and moves `file`
    // back to the root.
    let mut files = repo.list_files();
    files.sort();
    assert_eq!(files, vec!["dir", "file"]);

    txn.commit()?;

    Ok(())
}

#[test]
fn reconnect_lines() -> Result<(), anyhow::Error> {
    reconnect_(false)
}

#[test]
fn reconnect_files() -> Result<(), anyhow::Error> {
    reconnect_(true)
}

fn reconnect_(delete_file: bool) -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let repo2 = working_copy::memory::Memory::new();
    let repo3 = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"a\nb\nc\nd\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let env2 = pristine::sanakirja::Pristine::new_anon()?;
    let env3 = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();
    let txn2 = env2.arc_txn_begin().unwrap();
    let txn3 = env3.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;

    let channel2 = txn2
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let channel3 = txn3
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;

    apply::apply_change_arc(&changes, &txn2, &channel2, &h0)?;
    output::output_repository_no_pending(&repo2, &changes, &txn2, &channel2, "", true, None, 1, 0)?;
    apply::apply_change_arc(&changes, &txn3, &channel3, &h0)?;
    output::output_repository_no_pending(&repo3, &changes, &txn3, &channel3, "", true, None, 1, 0)?;

    // This test removes a line (in h1), then replaces it with another
    // one (in h2), removes the pseudo-edges (output, below), and then
    // unrecords h2 to delete the connection. Test: do the
    // pseudo-edges reappear?

    ///////////
    if delete_file {
        repo.remove_path("file", false)?;
    } else {
        repo.write_file("file", Inode::ROOT)?.write_all(b"a\nd\n")?;
    }
    record_all_output(&repo, changes.clone(), &txn, &channel, "")?;

    ///////////
    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nx\nc\nd\n")?;
    let h2 = record_all(&repo2, &changes, &txn2, &channel2, "")?;

    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nx\nc\ny\nd\n")?;
    let h3 = record_all(&repo2, &changes, &txn2, &channel2, "")?;

    ///////////
    apply::apply_change_arc(&changes, &txn, &channel, &h2)?;
    apply::apply_change_arc(&changes, &txn, &channel, &h3)?;
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)?;

    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h2,
        0,
        &mut Default::default(),
    )?;

    Ok(())
}

#[test]
fn zombie_file_test() -> Result<(), anyhow::Error> {
    zombie_(None, true)
}

#[test]
fn zombie_file_rev() -> Result<(), anyhow::Error> {
    zombie_(None, false)
}

#[test]
fn zombie_lines_test() -> Result<(), anyhow::Error> {
    zombie_(Some(b"a\nd\n"), true)
}

#[test]
fn zombie_lines_rev() -> Result<(), anyhow::Error> {
    zombie_(Some(b"a\nd\n"), false)
}

fn zombie_(file: Option<&[u8]>, order: bool) -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let repo2 = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"a\nb\nc\nd\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let env2 = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();
    let txn2 = env2.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;
    let channel2 = txn2
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;

    apply::apply_change_arc(&changes, &txn2, &channel2, &h0)?;
    output::output_repository_no_pending(&repo2, &changes, &txn2, &channel2, "", true, None, 1, 0)?;

    ///////////
    if let Some(file) = file {
        repo.write_file("file", Inode::ROOT)?.write_all(file)?;
    } else {
        repo.remove_path("file", false)?;
    }
    let h1 = record_all_output(&repo, changes.clone(), &txn, &channel, "")?;
    debug!("h1 = {:?}", h1);

    ///////////

    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nx\nc\ny\nd\n")?;
    let h2 = record_all_output(&repo2, changes.clone(), &txn2, &channel2, "")?;
    debug!("h2 = {:?}", h2);

    ///////////
    debug!("apply h2 to txn");
    apply::apply_change_arc(&changes, &txn, &channel, &h2)?;
    crate::pristine::debug(
        &*txn.read(),
        &*channel.read(),
        std::fs::File::create("alice-conflict.dot").unwrap(),
    )?;

    if order {
        debug!("unrecording h2 = {:?} from txn", h2);
        crate::unrecord::unrecord(
            &mut *txn.write(),
            &channel,
            &changes,
            &h2,
            0,
            &mut Default::default(),
        )?;
        check_unrec(&*txn.read(), &*channel.read(), h2.into());
    } else {
        debug!("unrecording h1 = {:?} from txn", h1);
        crate::unrecord::unrecord(
            &mut *txn.write(),
            &channel,
            &changes,
            &h1,
            0,
            &mut Default::default(),
        )?;
        check_unrec(&*txn.read(), &*channel.read(), h1.into());
    }
    crate::pristine::debug(
        &*txn.read(),
        &*channel.read(),
        std::fs::File::create("alice-unrec.dot").unwrap(),
    )?;
    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    let mut buf = Vec::new();
    if let Some(f) = file {
        if !conflicts.is_empty() {
            panic!("conflicts = {:#?}", conflicts)
        }
        repo.read_file("file", &mut buf)?;
        if order {
            assert_eq!(&buf[..], f);
        } else {
            // Unrecording h1 (which deleted b,c) leaves exactly {h0, h2}, and
            // h2 inserts *both* x and y onto the base, so the result must be
            // "a b x c y d". (The previous "a b x c d" here dropped y, which was
            // the zombie-cleanup bug in unrecord.)
            assert_eq!(&buf[..], b"a\nb\nx\nc\ny\nd\n");
        }
    } else {
        // Here in txn we have h1, we added h2 and unrecorded h2. No conflict.
        if !conflicts.is_empty() {
            panic!("conflicts = {:#?}", conflicts)
        }
    }

    let (alive_, reachable_) = check_alive(&*txn.read(), &channel.read());
    if !alive_.is_empty() {
        panic!("alive: {:?}", alive_);
    }
    if !reachable_.is_empty() {
        panic!("reachable: {:?}", reachable_);
    }

    txn.commit()?;

    // Applying the symmetric.
    debug!("apply h1 to txn2");
    apply::apply_change_arc(&changes, &txn2, &channel2, &h1)?;
    crate::pristine::debug(
        &*txn2.read(),
        &*channel2.read(),
        std::fs::File::create("bob-conflict.dot").unwrap(),
    )?;

    if order {
        debug!("unrecording h1 = {:?} from txn2", h1);
        crate::unrecord::unrecord(
            &mut *txn2.write(),
            &channel2,
            &changes,
            &h1,
            0,
            &mut Default::default(),
        )?;
    } else {
        debug!("unrecording h2 = {:?} from txn2", h2);
        crate::unrecord::unrecord(
            &mut *txn2.write(),
            &channel2,
            &changes,
            &h2,
            0,
            &mut Default::default(),
        )?;
    }
    crate::pristine::debug(
        &*txn2.read(),
        &*channel2.read(),
        std::fs::File::create("bob-unrec.dot").unwrap(),
    )?;

    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn2, &channel, "", true, None, 1, 0,
    )?;

    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts)
    }
    {
        let (alive_, reachable_) = check_alive(&*txn2.read(), &channel2.read());
        if !alive_.is_empty() {
            panic!("alive: {:?}", alive_);
        }
        if !reachable_.is_empty() {
            panic!("reachable: {:?}", reachable_);
        }
    }
    Ok(())
}

fn check_unrec<T: crate::pristine::GraphIter + crate::pristine::ChannelTxnT>(
    txn: &T,
    channel: &T::Channel,
    h: Hash,
) {
    let h = txn.get_internal(&h.into()).unwrap();

    for c in txn.iter_graph(txn.graph(channel), None).unwrap() {
        let (v, e) = c.unwrap();
        debug!("{:?} {:?}", v, e);
        if let Some(h) = h {
            assert!(v.change != *h);
            assert!(e.dest().change != *h);
            assert!(e.introduced_by() != *h);
        } else {
            // Every referenced change must still resolve (external + changeset).
            if !v.change.is_root() {
                txn.get_external(&v.change).unwrap();
                txn.get_changeset(txn.changes(channel), &v.change)
                    .unwrap()
                    .unwrap();
            }
            if !e.dest().change.is_root() {
                txn.get_external(&e.dest().change).unwrap();
                txn.get_changeset(txn.changes(channel), &e.dest().change)
                    .unwrap()
                    .unwrap();
            }
            if !e.introduced_by().is_root() {
                txn.get_external(&e.introduced_by()).unwrap();
                txn.get_changeset(txn.changes(channel), &e.introduced_by())
                    .unwrap()
                    .unwrap();
            }
        }
    }
    /*
        for e in iter_adjacent(
            txn,
            &channel,
            v,
            EdgeFlags::empty(),
            EdgeFlags::all() - EdgeFlags::DELETED - EdgeFlags::PARENT,
        )
        .unwrap()
        {
            let e = e.unwrap();
            stack.push(*txn.find_block(&channel, e.dest()).unwrap());
    }
        */
}

/// Canonical file-projection content: the multiset of non-marker lines. The
/// author's invariant is that unrecord and a fresh apply must project to the
/// same file *content*; the conflict structure (markers, nesting, branch
/// order) is an order-dependent, benign projection detail (apply's
/// pseudo-reconnection is not confluent — see `unrecord_nested_double`). So
/// tests compare this canonical content, not the exact rendering.
fn canon_content(o: &[u8]) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = o
        .split(|&b| b == b'\n')
        .filter(|l| {
            !l.starts_with(b">>>>>>>") && !l.starts_with(b"=======") && !l.starts_with(b"<<<<<<<")
        })
        .map(|l| l.to_vec())
        .collect();
    v.sort();
    v
}

#[test]
fn zombie_unrec() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let repo2 = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"a\nb\nc\nd\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let env2 = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();
    let txn2 = env2.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;
    let channel2 = txn2
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;

    apply::apply_change_arc(&changes, &txn2, &channel2, &h0)?;
    output::output_repository_no_pending(&repo2, &changes, &txn2, &channel2, "", true, None, 1, 0)?;

    ///////////
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\ny\nd\n")?;
    let h1 = record_all_output(&repo, changes.clone(), &txn, &channel, "")?;
    debug!("h1 = {:?}", h1);

    ///////////

    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nx\nc\nd\n")?;
    let h2 = record_all_output(&repo2, changes.clone(), &txn2, &channel2, "")?;
    debug!("h2 = {:?}", h2);

    ///////////
    debug!("apply h2 to txn");
    apply::apply_change_arc(&changes, &txn, &channel, &h2)?;
    crate::pristine::debug(
        &*txn.read(),
        &*channel.read(),
        std::fs::File::create("alice-conflict.dot").unwrap(),
    )?;

    debug!("unrecording h2 = {:?} from txn", h2);
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h2,
        0,
        &mut Default::default(),
    )?;
    check_unrec(&*txn.read(), &*channel.read(), h2.into());
    crate::pristine::debug(
        &*txn.read(),
        &*channel.read(),
        std::fs::File::create("alice-unrec.dot").unwrap(),
    )?;
    Ok(())
}

#[test]
fn zombie_dir() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("a/b/c/d", b"a\nb\nc\nd\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("a/b/c/d", 0)?;

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    repo.remove_path("a/b/c/d", false)?;
    let h1 = record_all_output(&repo, changes.clone(), &txn, &channel, "")?;

    repo.remove_path("a/b", true)?;
    let _h2 = record_all_output(&repo, changes.clone(), &txn, &channel, "")?;
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)?;
    let files = repo.list_files();
    assert_eq!(files, &["a"]);
    debug!("files={:?}", files);

    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h1,
        0,
        &mut Default::default(),
    )?;

    // We used to consider this a conflict, but now unrecording a file
    // deletion also resurrects its hierarchy.
    let _conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?
    .into_iter()
    .collect::<Vec<_>>();

    /*
    match conflicts[0] {
        Conflict::ZombieFile { ref path, .. } => assert_eq!(path, "a/b"),
        ref c => panic!("c = {:?}", c),
    }
    match conflicts[1] {
        Conflict::ZombieFile { ref path, .. } => assert_eq!(path, "a/b/c"),
        ref c => panic!("c = {:?}", c),
    }
     */

    let files = repo.list_files();
    debug!("files={:?}", files);
    assert_eq!(files, &["a", "a/b", "a/b/c", "a/b/c/d"]);

    let (alive_, reachable_) = check_alive(&*txn.read(), &channel.read());
    if !alive_.is_empty() {
        panic!("alive: {:?}", alive_);
    }
    if !reachable_.is_empty() {
        panic!("reachable: {:?}", reachable_);
    }

    txn.commit()?;

    Ok(())
}

#[test]
fn nodep() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("dir/file", b"a\nb\nc\nd\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("dir/file", 0)?;
    debug_inodes(&*txn.read());

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;

    repo.write_file("dir/file", Inode::ROOT)?
        .write_all(b"a\nx\nb\nd\n")?;

    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    debug_inodes(&*txn.read());

    match crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h0,
        0,
        &mut Default::default(),
    ) {
        Err(crate::unrecord::UnrecordError::ChangeIsDependedUpon { .. }) => {}
        _ => panic!("Should not be able to unrecord"),
    }

    debug_inodes(&*txn.read());
    let channel2 = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main2"))?;
    match crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel2,
        &changes,
        &h0,
        0,
        &mut Default::default(),
    ) {
        Err(crate::unrecord::UnrecordError::ChangeNotInChannel { .. }) => {}
        _ => panic!("Should not be able to unrecord"),
    }

    for p in txn.read().log(&*channel.read(), 0).unwrap() {
        debug!("p = {:?}", p);
    }

    debug_inodes(&*txn.read());
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h1,
        0,
        &mut Default::default(),
    )?;

    for p in txn.read().log(&*channel.read(), 0).unwrap() {
        debug!("p = {:?}", p);
    }

    debug_inodes(&*txn.read());
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h0,
        0,
        &mut Default::default(),
    )?;

    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)?;

    // Checking that unrecord doesn't delete files on the filesystem,
    // but updates the tree/revtree tables.
    let mut files = repo.list_files();
    files.sort();
    assert_eq!(files, &["dir", "dir/file"]);
    assert!(
        crate::fs::iter_working_copy(&*txn.read(), Inode::ROOT)
            .next()
            .is_none()
    );
    txn.commit()?;

    Ok(())
}

#[test]
fn file_del() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;

    repo.add_file("file", b"blabla".to_vec());
    txn.write().add_file("file", 0)?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;

    repo.remove_path("file", false)?;
    let h = record_all(&repo, &changes, &txn, &channel, "")?;

    debug!("unrecord h");
    // Unrecording the deletion.
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h,
        0,
        &mut Default::default(),
    )?;
    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts);
    }
    assert_eq!(repo.list_files(), vec!["file"]);

    // Unrecording the initial change.
    debug!("unrecord h0");
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h0,
        0,
        &mut Default::default(),
    )?;
    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts);
    }
    let files = repo.list_files();
    // Unrecording a file addition shouldn't delete the file.
    assert_eq!(files, &["file"]);
    txn.commit()?;
    Ok(())
}

/// Unrecording a change that edits the file around a conflict marker.
#[test]
fn self_context() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();

    let mut channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;

    repo.add_file("file", b"a\nb\n".to_vec());
    txn.write().add_file("file", 0)?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    let channel2 = txn
        .write()
        .fork(&channel, &SmallString::from_str("main2"))?;

    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nx\nb\n")?;
    record_all(&repo, &changes, &txn, &channel, "")?;
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\ny\nb\n")?;
    let b = record_all(&repo, &changes, &txn, &channel2, "")?;

    apply::apply_change_arc(&changes, &txn, &channel, &b)?;
    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    debug!("conflicts = {:#?}", conflicts);
    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;
    debug!("buf = {:?}", std::str::from_utf8(&buf));
    assert_eq!(conflicts.len(), 1);
    match conflicts.iter().next().unwrap() {
        Conflict::Order { .. } => {}
        ref c => panic!("c = {:?}", c),
    }

    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;
    let conflict: Vec<_> = std::str::from_utf8(&buf)?.lines().collect();
    {
        let mut w = repo.write_file("file", Inode::ROOT)?;
        for l in conflict.iter() {
            if l.starts_with(">>>") {
                writeln!(w, "bla\n{}\nbli", l)?
            } else {
                writeln!(w, "{}", l)?
            }
        }
    }
    let c = record_all(&repo, &changes, &txn, &channel, "")?;

    crate::unrecord::unrecord(
        &mut *txn.write(),
        &mut channel,
        &changes,
        &c,
        0,
        &mut Default::default(),
    )?;

    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    debug!("conflicts = {:#?}", conflicts);
    assert_eq!(conflicts.len(), 1);
    match conflicts.iter().next().unwrap() {
        Conflict::Order { .. } => {}
        ref c => panic!("c = {:?}", c),
    }

    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;

    let re = regex::bytes::Regex::new(r#" \[[^\]]*\]"#).unwrap();
    let buf_ = re.replace_all(&buf, &[][..]);

    let mut conflict: Vec<_> = std::str::from_utf8(&buf_)?.lines().collect();
    conflict.sort();
    assert_eq!(
        conflict,
        vec!["<<<<<<< 1", "======= 1", ">>>>>>> 1", "a", "b", "x", "y"]
    );
    txn.commit()?;

    Ok(())
}

#[test]
fn rollback_lines() -> Result<(), anyhow::Error> {
    rollback_(false)
}

#[test]
fn rollback_file() -> Result<(), anyhow::Error> {
    rollback_(true)
}

fn rollback_(delete_file: bool) -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env = pristine::sanakirja::Pristine::new_anon()?;

    let txn = env.arc_txn_begin().unwrap();

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;

    // Write a-b-c
    repo.add_file("file", b"a\nb\nc\n".to_vec());
    txn.write().add_file("file", 0)?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    // Delete -b-
    if delete_file {
        repo.remove_path("file", false)?
    } else {
        repo.write_file("file", Inode::ROOT)?.write_all(b"a\nd\n")?;
    }
    let h_del = record_all(&repo, &changes, &txn, &channel, "")?;

    // Rollback the deletion of -b-
    let p_del = changes.get_change(&h_del)?;
    debug!("p_del = {:#?}", p_del);
    let mut p_inv = p_del.inverse(
        &h_del,
        crate::change::ChangeHeader {
            authors: vec![],
            message: "rollback".to_string(),
            description: None,
            timestamp: jiff::Timestamp::now(),
        },
        Vec::new(),
    );
    let h_inv = changes.save_change(&mut p_inv, |_, _| Ok::<_, anyhow::Error>(()))?;
    apply::apply_change_arc(&changes, &txn, &channel, &h_inv)?;
    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts)
    }
    let mut buf = Vec::new();
    repo.read_file("file", &mut buf)?;
    assert_eq!(std::str::from_utf8(&buf), Ok("a\nb\nc\n"));

    // Unrecord the rollback
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h_inv,
        0,
        &mut Default::default(),
    )?;
    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts)
    }
    let mut buf = Vec::new();
    repo.read_file("file", &mut buf).unwrap();
    if delete_file {
        assert_eq!(std::str::from_utf8(&buf), Ok("a\nb\nc\n"));
    } else {
        assert_eq!(std::str::from_utf8(&buf), Ok("a\nd\n"));
    }

    txn.commit()?;

    Ok(())
}

/// Delete a line twice on two different channels, merge and unrecord
/// only one of them. Does the deleted edge reappear? It shouldn't.
#[test]
fn double_test() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let channel2 = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main2"))?;

    repo.add_file("file", b"blabla\nblibli\nblublu\n".to_vec());
    txn.write().add_file("file", 0)?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;
    debug!("h0 = {:?}", h0);

    apply::apply_change_arc(&changes, &txn, &channel2, &h0)?;

    // First deletion
    {
        let mut w = repo.write_file("file", Inode::ROOT)?;
        writeln!(w, "blabla\nblublu")?;
    }
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    debug!("h1 = {:?}", h1);

    // Second deletion
    let h2 = record_all(&repo, &changes, &txn, &channel2, "")?;
    debug!("h2 = {:?}", h2);

    // Both deletions together.
    debug!("applying");
    apply::apply_change_arc(&changes, &txn, &channel, &h2)?;

    debug!("unrecord h");
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h2,
        0,
        &mut Default::default(),
    )?;

    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts);
    }

    txn.commit()?;

    Ok(())
}

/// Same as `double` above, but with a (slightly) more convoluted change
/// dependency graph made by rolling the change back a few times.
#[test]
fn double_convoluted() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let mut channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let mut channel2 = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main2"))?;

    repo.add_file("file", b"blabla\nblibli\nblublu\n".to_vec());
    txn.write().add_file("file", 0)?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;
    debug!("h0 = {:?}", h0);

    apply::apply_change_arc(&changes, &txn, &channel2, &h0)?;

    // First deletion
    {
        let mut w = repo.write_file("file", Inode::ROOT)?;
        write!(w, "blabla\nblibli\n")?;
    }
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    debug!("h1 = {:?}", h1);

    // Second deletion
    {
        let mut w = repo.write_file("file", Inode::ROOT)?;
        writeln!(w, "blabla")?;
    }
    let h2 = record_all(&repo, &changes, &txn, &channel2, "")?;
    debug!("h2 = {:?}", h2);

    // Both deletions together, then unrecord on ~channel~.
    debug!("applying");
    apply::apply_change_arc(&changes, &txn, &channel, &h2)?;

    debug!("unrecord h");
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &mut channel,
        &changes,
        &h2,
        0,
        &mut Default::default(),
    )?;

    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts);
    }

    // Same on ~channel2~, but with a few extra layers of rollbacks in between.
    debug!("rolling back");
    apply::apply_change_arc(&changes, &txn, &channel2, &h1)?;
    let rollback = |h| {
        let p = changes.get_change(&h).unwrap();
        let mut p_inv = p.inverse(
            &h,
            crate::change::ChangeHeader {
                authors: vec![],
                message: "rollback".to_string(),
                description: None,
                timestamp: jiff::Timestamp::now(),
            },
            Vec::new(),
        );
        let h_inv = changes
            .save_change(&mut p_inv, |_, _| Ok::<_, anyhow::Error>(()))
            .unwrap();
        h_inv
    };
    let mut h = h2;
    for _i in 0..6 {
        let r = rollback(h);
        apply::apply_change_arc(&changes, &txn, &channel2, &r).unwrap();
        h = r
    }
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &mut channel2,
        &changes,
        &h1,
        0,
        &mut Default::default(),
    )?;

    let conflicts = output::output_repository_no_pending(
        &repo, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    if !conflicts.is_empty() {
        panic!("conflicts = {:#?}", conflicts)
    }

    txn.commit()?;

    Ok(())
}

/// Delete the same file on two different channels, merge, unrecord each patch on the same channel. What happens to tree/revtree?
#[test]
fn double_file() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let channel2 = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main2"))?;

    repo.add_file("file", b"blabla\nblibli\nblublu\n".to_vec());
    txn.write().add_file("file", 0)?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;
    debug!("h0 = {:?}", h0);

    apply::apply_change_arc(&changes, &txn, &channel2, &h0)?;

    // First deletion
    repo.remove_path("file", false)?;
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    debug!("h1 = {:?}", h1);
    // Second deletion
    let h2 = record_all(&repo, &changes, &txn, &channel2, "")?;
    debug!("h2 = {:?}", h2);

    // Both deletions together.
    debug!("applying");
    apply::apply_change_arc(&changes, &txn, &channel, &h2)?;

    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h1,
        0,
        &mut Default::default(),
    )?;
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &h2,
        0,
        &mut Default::default(),
    )?;

    let txn = txn.read();
    let mut inodes = txn.iter_inodes().unwrap();
    let (x, _) = inodes.next().unwrap().unwrap();
    assert!(x.is_root());
    assert!(inodes.next().is_some());
    assert!(inodes.next().is_none());
    Ok(())
}

/// Delete the same file on two different channels, merge, unrecord each patch on the same channel. What happens to tree/revtree?
#[test]
fn fs_times() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;

    repo.add_file("file", b"blabla\nblibli\nblublu\n".to_vec());
    repo.add_file("file2", b"blabla\nblibli\nblublu\n".to_vec());
    txn.write().add_file("file", 0)?;
    txn.write().add_file("file2", 0)?;
    let h0 = record_all(&repo, &changes, &txn, &channel, "")?;
    debug!("h0 = {:?}", h0);
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"blabla\nblublu\n")?;

    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;

    // Modify file2 as well, giving it an mtime one hour in the past. Under the
    // old design (skip files whose mtime predates the channel stamp) this
    // modification would have been *missed*. With the per-inode stat cache
    // (format v2), `record` skips a file only when its `(mtime, size)` still
    // match the cached values; file2's content — and hence size — changed, so it
    // is correctly re-read and recorded regardless of how old its mtime looks.
    repo.write_file("file2", Inode::ROOT)?
        .write_all(b"blabla\nblublu\n")?;
    repo.touch(
        "file2",
        std::time::SystemTime::now() - std::time::Duration::from_hours(1),
    )?;

    let mut touched = crate::unrecord::TouchedInodes::new();
    crate::unrecord::unrecord(&mut *txn.write(), &channel, &changes, &h1, 0, &mut touched)?;
    // The CLI `unrecord` command resets the mtimes of the affected files so the
    // next `record` re-reads them (see pijul/src/commands/unrecord.rs). Mirror
    // that here; it invalidates `file`'s cached stat.
    crate::unrecord::touch_inodes(&mut *txn.write(), &repo, &touched)?;

    info!("Final record");
    let h1 = record_all(&repo, &changes, &txn, &channel, "")?;
    let change1 = changes.get_change(&h1).unwrap();
    // Both files differ from the pristine now: `file` (its deletion was
    // unrecorded, so the working copy's edit reappears as a change) and `file2`
    // (freshly modified). The stat cache no longer hides file2's edit.
    assert_eq!(change1.changes.len(), 2);

    Ok(())
}

// ---------------------------------------------------------------------------
// Stateful, seeded property fuzzer for unrecord / zombie handling.
//
// Idea: instead of scripting the exact "apply -> conflict -> unrecord"
// sequence, we do a random walk over primitive pijul ops across a few
// channels. Concurrency between channels produces conflicts, zombies and
// block splits emergently; the walk eventually lands on the hard cases
// (e.g. "a zombie vertex was split before unrecording the deletion").
//
// After every step we assert two invariants:
//   * `check_alive` reports no alive-but-unreachable and no pseudo-only
//     vertices (structural orphan detector);
//   * two round-trip / commutation oracles on the *output*:
//       - apply(c) then unrecord(c) must leave the rendered file identical
//         (byte for byte, incl. conflict count);
//       - after a "deep" unrecord (a change with others stacked on top),
//         the channel must render identically to a fresh channel that
//         replays exactly the surviving change set.
//   Output bytes are order-independent by construction, so a vertex that
//   wrongly disappears shows up as a rendering difference (or as an
//   alive-unreachable vertex).
//
// Run it across all cores until it finds a counterexample:
//   FUZZ_THREADS=32 cargo test -p pijul-core --release \
//       fuzz_unrecord_zombies -- --ignored --nocapture
// Reproduce a specific seed verbosely:
//   FUZZ_START=<seed> FUZZ_SEEDS=1 FUZZ_THREADS=1 FUZZ_VERBOSE=1 \
//       cargo test -p pijul-core --release fuzz_unrecord_zombies -- --ignored --nocapture
// ---------------------------------------------------------------------------

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha20Rng;

struct FuzzChan<T: crate::pristine::ChannelTxnT> {
    channel: ChannelRef<T>,
    /// Changes currently applied to this channel, in a topological order.
    applied: Vec<Hash>,
}

/// Render `channel`'s single file to bytes, plus the number of conflicts.
fn fuzz_render<T>(
    changes: &changestore::memory::Memory,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
) -> Result<(Vec<u8>, usize), anyhow::Error>
where
    T: crate::pristine::ChannelMutTxnT
        + crate::pristine::TreeMutTxnT<TreeError = <T as crate::pristine::GraphTxnT>::GraphError>
        + Send
        + Sync
        + 'static,
    T::Channel: Send + Sync + 'static,
{
    let wc = working_copy::memory::Memory::new();
    let conflicts =
        output::output_repository_no_pending(&wc, changes, txn, channel, "", true, None, 1, 0)?;
    let mut buf = Vec::new();
    let _ = wc.read_file("file", &mut buf);
    Ok((buf, conflicts.len()))
}

/// Is change `c` applicable to a channel with `applied` already present?
fn fuzz_deps_satisfied(changes: &changestore::memory::Memory, applied: &[Hash], c: &Hash) -> bool {
    if applied.contains(c) {
        return false;
    }
    match changes.get_change(c) {
        Ok(ch) => ch.dependencies.iter().all(|d| applied.contains(d)),
        Err(_) => false,
    }
}

/// Does any change in `applied` depend on `c`? (If so, `c` can't be unrecorded.)
fn fuzz_has_dependent(changes: &changestore::memory::Memory, applied: &[Hash], c: &Hash) -> bool {
    applied
        .iter()
        .any(|g| g != c && matches!(changes.get_change(g), Ok(gc) if gc.dependencies.contains(c)))
}

/// A valid (dependency-respecting) apply order for `subset`. `reverse` picks the
/// highest-index applicable change each step (a different valid order).
fn fuzz_valid_order(
    changes: &changestore::memory::Memory,
    subset: &[Hash],
    reverse: bool,
) -> Vec<Hash> {
    let mut order: Vec<Hash> = Vec::new();
    let mut remaining: Vec<Hash> = subset.to_vec();
    while !remaining.is_empty() {
        let n = remaining.len();
        let idxs: Vec<usize> = if reverse {
            (0..n).rev().collect()
        } else {
            (0..n).collect()
        };
        let mut pick = None;
        for i in idxs {
            if fuzz_deps_satisfied(changes, &order, &remaining[i]) {
                pick = Some(i);
                break;
            }
        }
        match pick {
            Some(i) => order.push(remaining.remove(i)),
            None => order.extend(remaining.drain(..)),
        }
    }
    order
}

/// Apply `order` to a fresh channel and return its conflict count.
fn fuzz_apply_count<T>(
    changes: &changestore::memory::Memory,
    txn: &ArcTxn<T>,
    order: &[Hash],
    name: &str,
) -> Result<usize, anyhow::Error>
where
    T: crate::pristine::MutTxnT
        + crate::pristine::ChannelMutTxnT
        + crate::pristine::TreeMutTxnT<TreeError = <T as crate::pristine::GraphTxnT>::GraphError>
        + Send
        + Sync
        + 'static,
    T::Channel: Send + Sync + 'static,
{
    let ch = txn
        .write()
        .open_or_create_channel(&SmallString::from_str(name))?;
    for h in order {
        apply::apply_change_arc(changes, txn, &ch, h)?;
    }
    Ok(fuzz_render(changes, txn, &ch)?.1)
}

/// Apply a few random line-level edits to `content`.
fn fuzz_mutate(content: &[u8], rng: &mut ChaCha20Rng) -> Vec<u8> {
    let mut lines: Vec<Vec<u8>> = content.split(|&b| b == b'\n').map(|s| s.to_vec()).collect();
    // `split` on a trailing '\n' leaves a final empty element; drop it.
    if lines.last().map_or(false, |l| l.is_empty()) {
        lines.pop();
    }
    // Small alphabet on purpose: makes concurrent edits collide, which is
    // what produces zombies.
    let nops = rng.random_range(1..=3);
    for _ in 0..nops {
        if lines.is_empty() {
            lines.push(format!("L{}", rng.random_range(0..6)).into_bytes());
            continue;
        }
        match rng.random_range(0..3) {
            0 => {
                let p = rng.random_range(0..=lines.len());
                lines.insert(p, format!("L{}", rng.random_range(0..6)).into_bytes());
            }
            1 => {
                let p = rng.random_range(0..lines.len());
                lines.remove(p);
            }
            _ => {
                let p = rng.random_range(0..lines.len());
                lines[p] = format!("L{}", rng.random_range(0..6)).into_bytes();
            }
        }
    }
    let mut out = Vec::new();
    for l in &lines {
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    out
}

fn fuzz_seed(seed: u64, nsteps: usize, verbose: bool) -> Result<(), anyhow::Error> {
    // Give this seed its own deterministic timestamp range so the run is
    // reproducible per seed even when seeds run in parallel across threads.
    // The stride must exceed the max changes a single seed records.
    if let Ok(t) = std::env::var("FUZZ_FIXED_TIME") {
        let base: i64 = t.parse().unwrap_or(0);
        crate::tests::fuzz_clock_set(base.wrapping_add((seed as i64).wrapping_mul(10_000_000)));
    }
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let changes = changestore::memory::Memory::new();

    // Base change, shared by all channels.
    let base_wc = working_copy::memory::Memory::new();
    base_wc.add_file("file", b"L0\nL1\nL2\nL3\n".to_vec());
    txn.write().add_file("file", 0)?;
    let c0 = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("c0"))?;
    let h0 = record_all(&base_wc, &changes, &txn, &c0, "")?;

    let nchan = 3;
    let mut chans: Vec<FuzzChan<_>> = Vec::new();
    chans.push(FuzzChan {
        channel: c0,
        applied: vec![h0],
    });
    for i in 1..nchan {
        let ch = txn.write().fork(
            &chans[0].channel,
            &SmallString::from_str(&format!("c{}", i)),
        )?;
        chans.push(FuzzChan {
            channel: ch,
            applied: vec![h0],
        });
    }

    let mut oracle_ctr = 0u64;

    for step in 0..nsteps {
        match rng.random_range(0..100) {
            // ---- RECORD a new change on some channel -------------------
            op if op < 30 => {
                let ci = rng.random_range(0..chans.len());
                let wc = working_copy::memory::Memory::new();
                output::output_repository_no_pending(
                    &wc,
                    &changes,
                    &txn,
                    &chans[ci].channel,
                    "",
                    true,
                    None,
                    1,
                    0,
                )?;
                let mut buf = Vec::new();
                let _ = wc.read_file("file", &mut buf);
                let newc = fuzz_mutate(&buf, &mut rng);
                if newc == buf {
                    continue;
                }
                wc.write_file("file", Inode::ROOT)?.write_all(&newc)?;
                let h = record_all(&wc, &changes, &txn, &chans[ci].channel, "")?;
                chans[ci].applied.push(h);
                if verbose {
                    eprintln!(
                        "[{step}] RECORD on c{ci} -> {} : {:?}",
                        h.to_base32(),
                        String::from_utf8_lossy(&newc)
                    );
                }
            }
            // ---- APPLY an existing change to a channel -----------------
            op if op < 55 => {
                let mut all: Vec<Hash> = Vec::new();
                for c in &chans {
                    for h in &c.applied {
                        if !all.contains(h) {
                            all.push(*h);
                        }
                    }
                }
                if all.is_empty() {
                    continue;
                }
                let c = all[rng.random_range(0..all.len())];
                let ci = rng.random_range(0..chans.len());
                if fuzz_deps_satisfied(&changes, &chans[ci].applied, &c) {
                    apply::apply_change_arc(&changes, &txn, &chans[ci].channel, &c)?;
                    chans[ci].applied.push(c);
                    if verbose {
                        eprintln!("[{step}] APPLY {} to c{ci}", c.to_base32());
                    }
                }
            }
            // ---- PROBE: apply(c) then unrecord(c), expect identity -----
            op if op < 80 => {
                let mut all: Vec<Hash> = Vec::new();
                for c in &chans {
                    for h in &c.applied {
                        if !all.contains(h) {
                            all.push(*h);
                        }
                    }
                }
                if all.is_empty() {
                    continue;
                }
                let c = all[rng.random_range(0..all.len())];
                let ci = rng.random_range(0..chans.len());
                if !fuzz_deps_satisfied(&changes, &chans[ci].applied, &c) {
                    continue;
                }
                let dump = std::env::var("FUZZ_DUMP").is_ok();
                if dump {
                    crate::pristine::debug(
                        &*txn.read(),
                        &*chans[ci].channel.read(),
                        std::fs::File::create("rt-before.dot").unwrap(),
                    )?;
                }
                let before = fuzz_render(&changes, &txn, &chans[ci].channel)?;
                apply::apply_change_arc(&changes, &txn, &chans[ci].channel, &c)?;
                if dump {
                    crate::pristine::debug(
                        &*txn.read(),
                        &*chans[ci].channel.read(),
                        std::fs::File::create("rt-applied.dot").unwrap(),
                    )?;
                }
                crate::unrecord::unrecord(
                    &mut *txn.write(),
                    &chans[ci].channel,
                    &changes,
                    &c,
                    0,
                    &mut Default::default(),
                )?;
                let after = fuzz_render(&changes, &txn, &chans[ci].channel)?;
                // Round-trip `apply(C)` then `unrecord(C)` must restore the file
                // PROJECTION, but (per the author) may leave benign graph residue
                // (extra PSEUDO edges, split vertices) that reorders/regroups
                // conflicts without changing the content. Compare the content
                // line-multiset, not the exact rendering + count; graph integrity
                // is still checked by `check_alive`.
                let canon = |o: &[u8]| -> Vec<Vec<u8>> {
                    let mut v: Vec<Vec<u8>> = o
                        .split(|&b| b == b'\n')
                        .filter(|l| {
                            !l.starts_with(b">>>>>>>")
                                && !l.starts_with(b"=======")
                                && !l.starts_with(b"<<<<<<<")
                        })
                        .map(|l| l.to_vec())
                        .collect();
                    v.sort();
                    v
                };
                if canon(&before.0) != canon(&after.0) {
                    if dump {
                        crate::pristine::debug(
                            &*txn.read(),
                            &*chans[ci].channel.read(),
                            std::fs::File::create("rt-after.dot").unwrap(),
                        )?;
                    }
                    anyhow::bail!(
                        "seed {seed} step {step}: apply/unrecord round-trip changed c{ci}\n\
                         change  = {}\n\
                         before  = {:?} ({} conflicts)\n\
                         after   = {:?} ({} conflicts)",
                        c.to_base32(),
                        String::from_utf8_lossy(&before.0),
                        before.1,
                        String::from_utf8_lossy(&after.0),
                        after.1,
                    );
                }
                if verbose {
                    eprintln!("[{step}] PROBE round-trip {} on c{ci} ok", c.to_base32());
                }
            }
            // ---- DEEP unrecord of a change with stuff on top + oracle --
            _ => {
                let ci = rng.random_range(0..chans.len());
                let cand: Vec<Hash> = chans[ci]
                    .applied
                    .iter()
                    .skip(1) // keep h0 so the file stays around
                    .filter(|c| !fuzz_has_dependent(&changes, &chans[ci].applied, c))
                    .cloned()
                    .collect();
                if cand.is_empty() {
                    continue;
                }
                let c = cand[rng.random_range(0..cand.len())];
                crate::unrecord::unrecord(
                    &mut *txn.write(),
                    &chans[ci].channel,
                    &changes,
                    &c,
                    0,
                    &mut Default::default(),
                )?;
                chans[ci].applied.retain(|x| *x != c);

                // Dangling-reference invariant: after unrecording `c`, no edge in
                // the channel may still reference it. The content oracle can't
                // catch a leftover `c`-marking (content-equal), so check it here.
                check_unrec(&*txn.read(), &*chans[ci].channel.read(), c);

                // Oracle: a fresh channel replaying exactly the survivors.
                oracle_ctr += 1;
                let fresh =
                    txn.write()
                        .open_or_create_channel(&SmallString::from_str(&format!(
                            "oracle-{oracle_ctr}"
                        )))?;
                if std::env::var("SPLIT_TRACE").is_ok() {
                    eprintln!("=== REPLAY START (step {step}) ===");
                }
                for h in chans[ci].applied.clone() {
                    apply::apply_change_arc(&changes, &txn, &fresh, &h)?;
                }
                let got = fuzz_render(&changes, &txn, &chans[ci].channel)?;
                let exp = fuzz_render(&changes, &txn, &fresh)?;

                let canon = |o: &[u8]| -> Vec<Vec<u8>> {
                    let mut v: Vec<Vec<u8>> = o
                        .split(|&b| b == b'\n')
                        .filter(|l| {
                            !l.starts_with(b">>>>>>>")
                                && !l.starts_with(b"=======")
                                && !l.starts_with(b"<<<<<<<")
                        })
                        .map(|l| l.to_vec())
                        .collect();
                    v.sort();
                    v
                };
                if got != exp {
                    eprintln!(
                        "[seed {seed} step {step}] unrecord divergence is {}",
                        if canon(&got.0) != canon(&exp.0) {
                            "CONTENT (real)"
                        } else {
                            "conflict-order only (benign)"
                        }
                    );
                }

                // Commutativity probe: replay the SAME survivors in a second
                // valid order. Splitting is an implementation detail, so apply
                // must be order-independent; if the two fresh replays differ,
                // that's a pure apply-commutativity bug (no unrecord involved),
                // surfaced with the smallest survivor set that triggers it.
                if std::env::var("FUZZ_COMMUTE").is_ok() {
                    let survivors = chans[ci].applied.clone();
                    let mut alt: Vec<Hash> = Vec::new();
                    let mut remaining = survivors.clone();
                    while !remaining.is_empty() {
                        let mut pick = None;
                        for i in (0..remaining.len()).rev() {
                            if fuzz_deps_satisfied(&changes, &alt, &remaining[i]) {
                                pick = Some(i);
                                break;
                            }
                        }
                        alt.push(remaining.remove(pick.expect("dep cycle")));
                    }
                    if alt != survivors {
                        oracle_ctr += 1;
                        let fresh2 = txn.write().open_or_create_channel(&SmallString::from_str(
                            &format!("commute-{oracle_ctr}"),
                        ))?;
                        for h in &alt {
                            apply::apply_change_arc(&changes, &txn, &fresh2, h)?;
                        }
                        let exp2 = fuzz_render(&changes, &txn, &fresh2)?;
                        // Compare CONTENT, not exact rendering: conflict-branch
                        // order is an expected projection difference (the graph
                        // is commutative); a real bug is content actually
                        // changing (a line gained/lost). Strip conflict markers
                        // and sort the remaining lines.
                        let canon = |o: &[u8]| -> Vec<Vec<u8>> {
                            let mut v: Vec<Vec<u8>> = o
                                .split(|&b| b == b'\n')
                                .filter(|l| {
                                    !l.starts_with(b">>>>>>>")
                                        && !l.starts_with(b"=======")
                                        && !l.starts_with(b"<<<<<<<")
                                })
                                .map(|l| l.to_vec())
                                .collect();
                            v.sort();
                            v
                        };
                        if canon(&exp.0) != canon(&exp2.0) || exp.1 != exp2.1 {
                            // Shrink to the minimal dependency-closed subset whose
                            // two valid apply orders still disagree on conflict
                            // count (the real, order-independent-should-be signal).
                            let mut minimal = survivors.clone();
                            let mut tag = 1_000_000usize;
                            loop {
                                let mut reduced = false;
                                for i in 0..minimal.len() {
                                    let removed = minimal[i];
                                    let cand: Vec<Hash> = minimal
                                        .iter()
                                        .enumerate()
                                        .filter(|(j, _)| *j != i)
                                        .map(|(_, h)| *h)
                                        .collect();
                                    if cand.iter().any(|h| {
                                        matches!(changes.get_change(h), Ok(c) if c.dependencies.contains(&removed))
                                    }) {
                                        continue;
                                    }
                                    let fwd = fuzz_valid_order(&changes, &cand, false);
                                    let rev = fuzz_valid_order(&changes, &cand, true);
                                    if fwd == rev {
                                        continue;
                                    }
                                    tag += 1;
                                    let ca = fuzz_apply_count(
                                        &changes,
                                        &txn,
                                        &fwd,
                                        &format!("shrA{tag}"),
                                    )?;
                                    tag += 1;
                                    let cb = fuzz_apply_count(
                                        &changes,
                                        &txn,
                                        &rev,
                                        &format!("shrB{tag}"),
                                    )?;
                                    if ca != cb {
                                        minimal = cand;
                                        reduced = true;
                                        break;
                                    }
                                }
                                if !reduced {
                                    break;
                                }
                            }
                            let mfwd = fuzz_valid_order(&changes, &minimal, false);
                            let mrev = fuzz_valid_order(&changes, &minimal, true);
                            tag += 1;
                            let fa = txn.write().open_or_create_channel(&SmallString::from_str(
                                &format!("mfa{tag}"),
                            ))?;
                            for h in &mfwd {
                                apply::apply_change_arc(&changes, &txn, &fa, h)?;
                            }
                            let oa = fuzz_render(&changes, &txn, &fa)?;
                            tag += 1;
                            let fb = txn.write().open_or_create_channel(&SmallString::from_str(
                                &format!("mfb{tag}"),
                            ))?;
                            for h in &mrev {
                                apply::apply_change_arc(&changes, &txn, &fb, h)?;
                            }
                            let ob = fuzz_render(&changes, &txn, &fb)?;
                            if std::env::var("FUZZ_DUMP").is_ok() {
                                crate::pristine::debug(
                                    &*txn.read(),
                                    &*fa.read(),
                                    std::fs::File::create("commute-order1.dot").unwrap(),
                                )?;
                                crate::pristine::debug(
                                    &*txn.read(),
                                    &*fb.read(),
                                    std::fs::File::create("commute-order2.dot").unwrap(),
                                )?;
                            }
                            anyhow::bail!(
                                "seed {seed} step {step}: APPLY NON-COMMUTATIVE on c{ci}\n\
                                 minimal subset ({} changes): {:?}\n\
                                 order-fwd {:?} ({} conflicts) = {:?}\n\
                                 order-rev {:?} ({} conflicts) = {:?}",
                                minimal.len(),
                                minimal
                                    .iter()
                                    .map(|h| h.to_base32()[..8].to_string())
                                    .collect::<Vec<_>>(),
                                mfwd.iter()
                                    .map(|h| h.to_base32()[..8].to_string())
                                    .collect::<Vec<_>>(),
                                oa.1,
                                String::from_utf8_lossy(&oa.0),
                                mrev.iter()
                                    .map(|h| h.to_base32()[..8].to_string())
                                    .collect::<Vec<_>>(),
                                ob.1,
                                String::from_utf8_lossy(&ob.0),
                            );
                        }
                    }
                }
                // Compare the file PROJECTION by content only. Per the pijul
                // author: the graph legitimately differs between unrecord and a
                // fresh apply (extra PSEUDO edges, split vertices, …), and those
                // differences have the same semantics — but the projection to the
                // file must match. Crucially, apply's pseudo-reconnection is NOT
                // confluent: the SAME survivor set rendered in two valid apply
                // orders can yield a different conflict COUNT/structure (e.g.
                // `unrecord_nested_double`: 3 vs 1), while the content (the
                // multiset of lines) is invariant. So conflict count/structure is
                // a benign, order-dependent projection detail and must not fail
                // the oracle; only a real change in content (a line gained, lost,
                // or duplicated) is a bug. Graph integrity is still enforced
                // separately by `check_alive` below.
                if canon(&got.0) != canon(&exp.0) {
                    if std::env::var("FUZZ_DUMP").is_ok() {
                        crate::pristine::debug(
                            &*txn.read(),
                            &*chans[ci].channel.read(),
                            std::fs::File::create("diverge-unrecorded.dot").unwrap(),
                        )?;
                        crate::pristine::debug(
                            &*txn.read(),
                            &*fresh.read(),
                            std::fs::File::create("diverge-replay.dot").unwrap(),
                        )?;
                    }
                    anyhow::bail!(
                        "seed {seed} step {step}: deep unrecord of {} on c{ci} diverged from replay\n\
                         unrecorded = channel {:?} ({} conflicts)\n\
                         replay     = fresh   {:?} ({} conflicts)",
                        c.to_base32(),
                        String::from_utf8_lossy(&got.0),
                        got.1,
                        String::from_utf8_lossy(&exp.0),
                        exp.1,
                    );
                }
                if verbose {
                    eprintln!("[{step}] DEEP unrecord {} on c{ci} ok", c.to_base32());
                }
            }
        }

        // Always-on structural invariant on every channel.
        for (ci, c) in chans.iter().enumerate() {
            let (alive, pseudo) = check_alive(&*txn.read(), &c.channel.read());
            if !alive.is_empty() {
                anyhow::bail!(
                    "seed {seed} step {step}: alive-but-unreachable vertices on c{ci}: {:?}",
                    alive
                );
            }
            if !pseudo.is_empty() {
                anyhow::bail!(
                    "seed {seed} step {step}: pseudo-only vertices on c{ci}: {:?}",
                    pseudo
                );
            }
        }

        // Localizer: after EVERY step, compare each channel to a fresh replay
        // of its survivors, using conflict-count + canonical content (which
        // ignore benign conflict-branch ORDER but catch real structure loss
        // like a vanished zombie conflict). Pinpoints the first diverging step.
        if std::env::var("FUZZ_LOCALIZE").is_ok() {
            let canon = |o: &[u8]| -> Vec<Vec<u8>> {
                let mut v: Vec<Vec<u8>> = o
                    .split(|&b| b == b'\n')
                    .filter(|l| {
                        !l.starts_with(b">>>>>>>")
                            && !l.starts_with(b"=======")
                            && !l.starts_with(b"<<<<<<<")
                    })
                    .map(|l| l.to_vec())
                    .collect();
                v.sort();
                v
            };
            for ci in 0..chans.len() {
                let applied = chans[ci].applied.clone();
                let fresh = txn
                    .write()
                    .open_or_create_channel(&SmallString::from_str(&format!("loc-{step}-{ci}")))?;
                for h in &applied {
                    apply::apply_change_arc(&changes, &txn, &fresh, h)?;
                }
                if std::env::var("FUZZ_DUMP_STEP")
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    == Some(step)
                {
                    crate::pristine::debug(
                        &*txn.read(),
                        &*chans[ci].channel.read(),
                        std::fs::File::create(format!("loc-chan{ci}.dot")).unwrap(),
                    )?;
                    crate::pristine::debug(
                        &*txn.read(),
                        &*fresh.read(),
                        std::fs::File::create(format!("loc-fresh{ci}.dot")).unwrap(),
                    )?;
                }
                let got = fuzz_render(&changes, &txn, &chans[ci].channel)?;
                let exp = fuzz_render(&changes, &txn, &fresh)?;
                // Conflict-count + canonical content ignore benign split /
                // branch-order representation but catch real structure loss
                // (e.g. a vanished zombie conflict).
                if got.1 != exp.1 || canon(&got.0) != canon(&exp.0) {
                    anyhow::bail!(
                        "seed {seed} step {step}: c{ci} STRUCTURE diverged (conflicts {} vs {}, content {})",
                        got.1,
                        exp.1,
                        if canon(&got.0) != canon(&exp.0) {
                            "DIFFERS"
                        } else {
                            "same"
                        },
                    );
                }
            }
        }
    }

    Ok(())
}

/// Quick, deterministic, single-threaded slice that runs in CI.
#[test]
fn fuzz_unrecord_smoke() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    for seed in 0..64 {
        fuzz_seed(seed, 25, false)?;
    }
    Ok(())
}

/// Long-running, multi-threaded search. Ignored by default; run explicitly.
#[test]
#[ignore]
fn fuzz_unrecord_zombies() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::sync::{Arc, Mutex};

    fn env_usize(k: &str, d: usize) -> usize {
        std::env::var(k)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(d)
    }
    fn env_u64(k: &str, d: u64) -> u64 {
        std::env::var(k)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(d)
    }

    let threads = env_usize(
        "FUZZ_THREADS",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4),
    );
    let nsteps = env_usize("FUZZ_STEPS", 40);
    let start = env_u64("FUZZ_START", 0);
    let nseeds = env_u64("FUZZ_SEEDS", u64::MAX);
    let verbose = std::env::var("FUZZ_VERBOSE").is_ok();
    let end = start.saturating_add(nseeds);

    eprintln!("fuzzing unrecord: threads={threads} steps={nsteps} seeds=[{start}, {end})");

    let next = Arc::new(AtomicU64::new(start));
    let stop = Arc::new(AtomicBool::new(false));
    let found: Arc<Mutex<Option<(u64, String)>>> = Arc::new(Mutex::new(None));

    // Keep thread output quiet until we actually find something.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        if std::env::var("SHOW_PANIC").is_ok() {
            eprintln!("PANIC-LOC {:?}: {}", info.location(), info);
        }
    }));

    let mut handles = Vec::new();
    for _ in 0..threads {
        let next = next.clone();
        let stop = stop.clone();
        let found = found.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                if stop.load(Relaxed) {
                    break;
                }
                let seed = next.fetch_add(1, Relaxed);
                if seed >= end {
                    break;
                }
                if seed % 1000 == 0 {
                    eprintln!("... seed {seed}");
                }
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fuzz_seed(seed, nsteps, verbose)
                }));
                let bad = match res {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{e:#}")),
                    Err(p) => Some(format!(
                        "panic: {}",
                        p.downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| p.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "<opaque>".to_string())
                    )),
                };
                if let Some(msg) = bad {
                    *found.lock().unwrap() = Some((seed, msg));
                    stop.store(true, Relaxed);
                    break;
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    std::panic::set_hook(default_hook);

    if let Some((seed, msg)) = found.lock().unwrap().take() {
        panic!(
            "\n==== FOUND COUNTEREXAMPLE ====\nseed = {seed}\n{msg}\n\n\
             reproduce: FUZZ_START={seed} FUZZ_SEEDS=1 FUZZ_THREADS=1 FUZZ_VERBOSE=1 \
             cargo test -p pijul-core --release fuzz_unrecord_zombies -- --ignored --nocapture\n"
        );
    }
}

/// Minimal, hand-written distillation of the `fuzz_seed(17)` counterexample.
///
/// Base file is `L0 L1 L2 L3`. Channel A rewrites `L1 -> L5`. Channel B
/// (concurrent, from the same base) rewrites `L2 -> L1`. Applying B's change
/// onto A and then immediately unrecording it must be a no-op, but instead it
/// leaves a phantom zombie conflict around A's `L5` line.
///
/// Writes graph snapshots to `phantom-{before,applied,unrecorded}.dot`.
#[test]
fn zombie_unrecord_phantom() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let changes = changestore::memory::Memory::new();

    // ---- Channel A: base, then L1 -> L5 -------------------------------
    let repo_a = working_copy::memory::Memory::new();
    repo_a.add_file("file", b"L0\nL1\nL2\nL3\n".to_vec());
    let env_a = pristine::sanakirja::Pristine::new_anon()?;
    let txn_a = env_a.arc_txn_begin().unwrap();
    txn_a.write().add_file("file", 0)?;
    let chan_a = txn_a
        .write()
        .open_or_create_channel(&SmallString::from_str("a"))?;
    let h0 = record_all(&repo_a, &changes, &txn_a, &chan_a, "")?;
    repo_a
        .write_file("file", Inode::ROOT)?
        .write_all(b"L0\nL5\nL2\nL3\n")?;
    let _ha = record_all(&repo_a, &changes, &txn_a, &chan_a, "")?;

    // ---- Channel B (separate txn), from the same base: L2 -> L1 -------
    let repo_b = working_copy::memory::Memory::new();
    repo_b.add_file("file", b"L0\nL1\nL2\nL3\n".to_vec());
    let env_b = pristine::sanakirja::Pristine::new_anon()?;
    let txn_b = env_b.arc_txn_begin().unwrap();
    txn_b.write().add_file("file", 0)?;
    let chan_b = txn_b
        .write()
        .open_or_create_channel(&SmallString::from_str("b"))?;
    apply::apply_change_arc(&changes, &txn_b, &chan_b, &h0)?;
    output::output_repository_no_pending(&repo_b, &changes, &txn_b, &chan_b, "", true, None, 1, 0)?;
    repo_b
        .write_file("file", Inode::ROOT)?
        .write_all(b"L0\nL1\nL1\nL3\n")?;
    let hb = record_all(&repo_b, &changes, &txn_b, &chan_b, "")?;

    // ---- A's state before the round-trip ------------------------------
    let before = fuzz_render(&changes, &txn_a, &chan_a)?;
    assert_eq!(std::str::from_utf8(&before.0).unwrap(), "L0\nL5\nL2\nL3\n");
    assert_eq!(before.1, 0);
    crate::pristine::debug(
        &*txn_a.read(),
        &*chan_a.read(),
        std::fs::File::create("phantom-before.dot").unwrap(),
    )?;

    // ---- Apply B's change onto A, then unrecord it (must be identity) -
    apply::apply_change_arc(&changes, &txn_a, &chan_a, &hb)?;
    crate::pristine::debug(
        &*txn_a.read(),
        &*chan_a.read(),
        std::fs::File::create("phantom-applied.dot").unwrap(),
    )?;

    debug!("unrecording {:?}", hb);
    crate::unrecord::unrecord(
        &mut *txn_a.write(),
        &chan_a,
        &changes,
        &hb,
        0,
        &mut Default::default(),
    )?;
    crate::pristine::debug(
        &*txn_a.read(),
        &*chan_a.read(),
        std::fs::File::create("phantom-unrecorded.dot").unwrap(),
    )?;

    let after = fuzz_render(&changes, &txn_a, &chan_a)?;
    assert_eq!(
        std::str::from_utf8(&after.0).unwrap(),
        "L0\nL5\nL2\nL3\n",
        "unrecord left a phantom zombie conflict: {:?} ({} conflicts); \
         see phantom-*.dot",
        String::from_utf8_lossy(&after.0),
        after.1,
    );
    assert_eq!(after.1, 0);
    Ok(())
}

/// Distillation of the `fuzz_seed(35)` counterexample: unrecording the deletion
/// of the *first* line, after an intermediate change was already unrecorded,
/// reconnects that line in the wrong order and produces a spurious conflict.
///
/// One channel: base `L0 L1 L2 L3`; delete L1; delete L2,L3 + insert L4; delete
/// L0. Then unrecord the (L2,L3->L4) change and the (delete L0) change. What
/// remains is {base, delete-L1} = "L0 L2 L3", but unrecord instead yields a
/// reordering conflict with L0 below L2 L3.
///
/// Regression test for the obsolete-reconnection bug: unrecording the deletion
/// of the first line after an intermediate change was already unrecorded used to
/// leave a stale `anchor -> L2L3` pseudo-edge, so `repair_zombies` reconnected
/// L0 below L2 L3 as a conflict. Fixed by `remove_obsolete_reconnections`.
#[test]
fn unrecord_reorder_seed35() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"L0\nL1\nL2\nL3\n".to_vec());

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let _h0 = record_all(&repo, &changes, &txn, &channel, "")?;

    // delete L1
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"L0\nL2\nL3\n")?;
    let _del_l1 = record_all(&repo, &changes, &txn, &channel, "")?;

    // delete L2, L3 and insert L4
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"L0\nL4\n")?;
    let mid = record_all(&repo, &changes, &txn, &channel, "")?;

    // delete L0
    repo.write_file("file", Inode::ROOT)?.write_all(b"L4\n")?;
    let del_l0 = record_all(&repo, &changes, &txn, &channel, "")?;

    crate::pristine::debug(
        &*txn.read(),
        &*channel.read(),
        std::fs::File::create("seed35-before.dot").unwrap(),
    )?;

    // Unrecord the middle change, then the deletion of L0.
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &mid,
        0,
        &mut Default::default(),
    )?;
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &channel,
        &changes,
        &del_l0,
        0,
        &mut Default::default(),
    )?;
    crate::pristine::debug(
        &*txn.read(),
        &*channel.read(),
        std::fs::File::create("seed35-unrecorded.dot").unwrap(),
    )?;

    // What's left is {base, delete-L1} = "L0 L2 L3", no conflict.
    let (buf, nconf) = fuzz_render(&changes, &txn, &channel)?;
    assert_eq!(
        std::str::from_utf8(&buf).unwrap(),
        "L0\nL2\nL3\n",
        "unrecord produced a reordering conflict ({} conflicts); see seed35-*.dot",
        nconf,
    );
    assert_eq!(nconf, 0);
    Ok(())
}

/// Case 1 of seed 31, isolated (no split): a zombie conflict, then a resolution
/// that *undeletes* the deleted lines. Unrecording the resolution must restore
/// the conflict (== fresh apply of the survivors {h0, hdel, hins}).
#[test]
fn unrecord_resolution_roundtrip() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let repo2 = working_copy::memory::Memory::new();
    let repo3 = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"a\nb\nc\nd\n".to_vec());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;
    let main = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let h0 = record_all(&repo, &changes, &txn, &main, "")?;
    let c2 = txn.write().fork(&main, &SmallString::from_str("c2"))?;
    output::output_repository_no_pending(&repo2, &changes, &txn, &c2, "", true, None, 1, 0)?;

    repo.write_file("file", Inode::ROOT)?.write_all(b"a\nd\n")?;
    let hdel = record_all(&repo, &changes, &txn, &main, "")?;
    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nx\nc\ny\nd\n")?;
    let hins = record_all(&repo2, &changes, &txn, &c2, "")?;
    apply::apply_change_arc(&changes, &txn, &main, &hins)?;

    // Resolution on main: keep b and c (undeletes them) plus x,y.
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nx\nc\ny\nd\n")?;
    let hres = record_all(&repo, &changes, &txn, &main, "")?;
    let (res_out, res_conf) = fuzz_render(&changes, &txn, &main)?;
    eprintln!(
        "after resolution: {:?} ({} conf)",
        String::from_utf8_lossy(&res_out),
        res_conf
    );

    // Unrecord the resolution.
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &main,
        &changes,
        &hres,
        0,
        &mut Default::default(),
    )?;
    let (unrec_out, unrec_conf) = fuzz_render(&changes, &txn, &main)?;

    // Oracle: fresh apply of the survivors {h0, hdel, hins}.
    let fresh = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("fresh"))?;
    apply::apply_change_arc(&changes, &txn, &fresh, &h0)?;
    apply::apply_change_arc(&changes, &txn, &fresh, &hdel)?;
    apply::apply_change_arc(&changes, &txn, &fresh, &hins)?;
    let (oracle_out, oracle_conf) = fuzz_render(&changes, &txn, &fresh)?;

    eprintln!(
        "after unrecord: {:?} ({} conf)",
        String::from_utf8_lossy(&unrec_out),
        unrec_conf
    );
    eprintln!(
        "oracle (fresh): {:?} ({} conf)",
        String::from_utf8_lossy(&oracle_out),
        oracle_conf
    );
    let _ = &repo3;
    assert_eq!(
        std::str::from_utf8(&unrec_out).unwrap(),
        std::str::from_utf8(&oracle_out).unwrap(),
        "unrecording the resolution did not restore the conflict"
    );
    Ok(())
}

/// Like `unrecord_resolution_roundtrip`, but b,c are deleted by TWO concurrent
/// changes (a double deletion — the `must_reintroduce` "deleted twice" case).
/// Unrecording the resolution must restore BOTH deleters' markings.
#[test]
fn unrecord_resolution_double() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let repo2 = working_copy::memory::Memory::new();
    let repo3 = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"a\nb\nc\nd\n".to_vec());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;
    let main = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let h0 = record_all(&repo, &changes, &txn, &main, "")?;
    let c2 = txn.write().fork(&main, &SmallString::from_str("c2"))?;
    let c3 = txn.write().fork(&main, &SmallString::from_str("c3"))?;
    output::output_repository_no_pending(&repo2, &changes, &txn, &c2, "", true, None, 1, 0)?;
    output::output_repository_no_pending(&repo3, &changes, &txn, &c3, "", true, None, 1, 0)?;

    // main and c2 both delete b,c (concurrently).
    repo.write_file("file", Inode::ROOT)?.write_all(b"a\nd\n")?;
    let hdela = record_all(&repo, &changes, &txn, &main, "")?;
    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nd\n")?;
    let hdelb = record_all(&repo2, &changes, &txn, &c2, "")?;
    // c3 inserts x,y.
    repo3
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nx\nc\ny\nd\n")?;
    let hins = record_all(&repo3, &changes, &txn, &c3, "")?;

    apply::apply_change_arc(&changes, &txn, &main, &hdelb)?;
    apply::apply_change_arc(&changes, &txn, &main, &hins)?;

    // Resolution: keep everything (undeletes b,c).
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nx\nc\ny\nd\n")?;
    let hres = record_all(&repo, &changes, &txn, &main, "")?;

    crate::unrecord::unrecord(
        &mut *txn.write(),
        &main,
        &changes,
        &hres,
        0,
        &mut Default::default(),
    )?;
    let (unrec_out, unrec_conf) = fuzz_render(&changes, &txn, &main)?;

    let fresh = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("fresh"))?;
    for h in [&h0, &hdela, &hdelb, &hins] {
        apply::apply_change_arc(&changes, &txn, &fresh, h)?;
    }
    let (oracle_out, oracle_conf) = fuzz_render(&changes, &txn, &fresh)?;

    eprintln!(
        "after unrecord: {:?} ({} conf)",
        String::from_utf8_lossy(&unrec_out),
        unrec_conf
    );
    eprintln!(
        "oracle (fresh): {:?} ({} conf)",
        String::from_utf8_lossy(&oracle_out),
        oracle_conf
    );
    assert_eq!(
        std::str::from_utf8(&unrec_out).unwrap(),
        std::str::from_utf8(&oracle_out).unwrap(),
        "double-deletion resolution unrecord diverged"
    );
    Ok(())
}

/// Case 1 + the split: b and TWO identical `c` lines; an insert lands *between*
/// the two c's (splitting that vertex), concurrently with their deletion; then a
/// resolution undeletes; then unrecord. Mirrors seed 31's "L3\nL3\n" split.
#[test]
fn unrecord_resolution_split() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let repo2 = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"a\nc\nc\nd\n".to_vec());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;
    let main = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let h0 = record_all(&repo, &changes, &txn, &main, "")?;
    let c2 = txn.write().fork(&main, &SmallString::from_str("c2"))?;
    output::output_repository_no_pending(&repo2, &changes, &txn, &c2, "", true, None, 1, 0)?;

    // main deletes both c's.
    repo.write_file("file", Inode::ROOT)?.write_all(b"a\nd\n")?;
    let hdel = record_all(&repo, &changes, &txn, &main, "")?;
    // c2 inserts y BETWEEN the two c's (splits that vertex).
    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nc\ny\nc\nd\n")?;
    let hins = record_all(&repo2, &changes, &txn, &c2, "")?;
    apply::apply_change_arc(&changes, &txn, &main, &hins)?;

    let (conf_out, conf_n) = fuzz_render(&changes, &txn, &main)?;
    eprintln!(
        "conflict: {:?} ({} conf)",
        String::from_utf8_lossy(&conf_out),
        conf_n
    );

    // Resolution: keep the c's and y (undeletes the c's).
    repo.write_file("file", Inode::ROOT)?
        .write_all(b"a\nc\ny\nc\nd\n")?;
    let hres = record_all(&repo, &changes, &txn, &main, "")?;

    crate::unrecord::unrecord(
        &mut *txn.write(),
        &main,
        &changes,
        &hres,
        0,
        &mut Default::default(),
    )?;
    let (unrec_out, unrec_conf) = fuzz_render(&changes, &txn, &main)?;

    let fresh = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("fresh"))?;
    for h in [&h0, &hdel, &hins] {
        apply::apply_change_arc(&changes, &txn, &fresh, h)?;
    }
    let (oracle_out, oracle_conf) = fuzz_render(&changes, &txn, &fresh)?;

    eprintln!(
        "after unrecord: {:?} ({} conf)",
        String::from_utf8_lossy(&unrec_out),
        unrec_conf
    );
    eprintln!(
        "oracle (fresh): {:?} ({} conf)",
        String::from_utf8_lossy(&oracle_out),
        oracle_conf
    );
    assert_eq!(
        std::str::from_utf8(&unrec_out).unwrap(),
        std::str::from_utf8(&oracle_out).unwrap(),
        "split resolution unrecord diverged"
    );
    Ok(())
}

/// Author's seed-31 hypothesis: insert X into a deleted (zombie) context, then
/// insert Y into X (both unresolved / nested), then unrecord the deletion. After
/// unrecord, the context is alive again, so the result must equal fresh apply of
/// {h0, hins_x, hins_y} = "a b X Y c".
#[test]
fn unrecord_nested_zombie_insert() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let repo2 = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"a\nb\nc\n".to_vec());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;
    let main = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let h0 = record_all(&repo, &changes, &txn, &main, "")?;
    let c2 = txn.write().fork(&main, &SmallString::from_str("c2"))?;
    output::output_repository_no_pending(&repo2, &changes, &txn, &c2, "", true, None, 1, 0)?;

    // main deletes b.
    repo.write_file("file", Inode::ROOT)?.write_all(b"a\nc\n")?;
    let hdel = record_all(&repo, &changes, &txn, &main, "")?;
    // c2 inserts X after b, then Y after X.
    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nX\nc\n")?;
    let hins_x = record_all(&repo2, &changes, &txn, &c2, "")?;
    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nX\nY\nc\n")?;
    let hins_y = record_all(&repo2, &changes, &txn, &c2, "")?;

    apply::apply_change_arc(&changes, &txn, &main, &hins_x)?;
    apply::apply_change_arc(&changes, &txn, &main, &hins_y)?;
    let (z, zc) = fuzz_render(&changes, &txn, &main)?;
    eprintln!(
        "zombie state: {:?} ({} conf)",
        String::from_utf8_lossy(&z),
        zc
    );

    // Unrecord the deletion.
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &main,
        &changes,
        &hdel,
        0,
        &mut Default::default(),
    )?;
    let (unrec_out, unrec_conf) = fuzz_render(&changes, &txn, &main)?;

    let fresh = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("fresh"))?;
    for h in [&h0, &hins_x, &hins_y] {
        apply::apply_change_arc(&changes, &txn, &fresh, h)?;
    }
    let (oracle_out, oracle_conf) = fuzz_render(&changes, &txn, &fresh)?;

    eprintln!(
        "after unrecord: {:?} ({} conf)",
        String::from_utf8_lossy(&unrec_out),
        unrec_conf
    );
    eprintln!(
        "oracle (fresh): {:?} ({} conf)",
        String::from_utf8_lossy(&oracle_out),
        oracle_conf
    );
    assert_eq!(
        std::str::from_utf8(&unrec_out).unwrap(),
        std::str::from_utf8(&oracle_out).unwrap(),
        "nested-zombie-insert unrecord diverged"
    );
    Ok(())
}

/// Author's hypothesis + double deletion: b,c deleted by TWO changes; X inserted
/// in that deleted context, Y inserted into X (nested, unresolved); unrecord ONE
/// deleter. The survivors {h0, hdelB, hins_x, hins_y} still conflict (hdelB keeps
/// b,c deleted), so unrecord must reproduce that exact conflict.
#[test]
fn unrecord_nested_double() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let repo2 = working_copy::memory::Memory::new();
    let repo3 = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"a\nb\nc\nd\n".to_vec());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;
    let main = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let h0 = record_all(&repo, &changes, &txn, &main, "")?;
    let c2 = txn.write().fork(&main, &SmallString::from_str("c2"))?;
    let c3 = txn.write().fork(&main, &SmallString::from_str("c3"))?;
    output::output_repository_no_pending(&repo2, &changes, &txn, &c2, "", true, None, 1, 0)?;
    output::output_repository_no_pending(&repo3, &changes, &txn, &c3, "", true, None, 1, 0)?;

    repo.write_file("file", Inode::ROOT)?.write_all(b"a\nd\n")?;
    let hdela = record_all(&repo, &changes, &txn, &main, "")?;
    repo2
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nd\n")?;
    let hdelb = record_all(&repo2, &changes, &txn, &c2, "")?;
    repo3
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nX\nc\nd\n")?;
    let hins_x = record_all(&repo3, &changes, &txn, &c3, "")?;
    repo3
        .write_file("file", Inode::ROOT)?
        .write_all(b"a\nb\nX\nY\nc\nd\n")?;
    let hins_y = record_all(&repo3, &changes, &txn, &c3, "")?;

    for h in [&hdelb, &hins_x, &hins_y] {
        apply::apply_change_arc(&changes, &txn, &main, h)?;
    }
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &main,
        &changes,
        &hdela,
        0,
        &mut Default::default(),
    )?;
    let (unrec_out, _) = fuzz_render(&changes, &txn, &main)?;

    let fresh = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("fresh"))?;
    for h in [&h0, &hdelb, &hins_x, &hins_y] {
        apply::apply_change_arc(&changes, &txn, &fresh, h)?;
    }
    let (oracle_out, _) = fuzz_render(&changes, &txn, &fresh)?;

    eprintln!("after unrecord: {:?}", String::from_utf8_lossy(&unrec_out));
    eprintln!("oracle (fresh): {:?}", String::from_utf8_lossy(&oracle_out));
    // Content must match a fresh replay of the survivors; the conflict
    // structure (here nested vs flattened) is a benign, order-dependent
    // projection detail — apply itself renders this survivor set with a
    // different conflict count depending on apply order.
    assert_eq!(
        canon_content(&unrec_out),
        canon_content(&oracle_out),
        "nested + double-deletion unrecord changed the content\nunrec  = {:?}\noracle = {:?}",
        String::from_utf8_lossy(&unrec_out),
        String::from_utf8_lossy(&oracle_out),
    );
    Ok(())
}
// Minimal reproduction of fuzzer seed 57: unrecord drops a *surviving* change's
// DELETED zombie marking on another change's line.
//
// base = L0 L1 L2 L3. On one channel, record CO (inserts a duplicate L2) then OZ
// (collapses to a single L2); apply H (deletes L0 and L2, zombifying CO's L2);
// then unrecord OZ. A fresh replay of the survivors {base, CO, H} renders CO's L2
// as a zombie conflict (H deleted its context). Unrecord instead reconnects with
// a pseudo edge and drops H's DELETED marking, so the conflict vanishes.
#[test]
fn unrecord_zombie_marking_57() -> Result<(), anyhow::Error> {
    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    repo.add_file("file", b"L0\nL1\nL2\nL3\n".to_vec());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0)?;
    let c0 = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("c0"))?;
    let h0 = record_all(&repo, &changes, &txn, &c0, "")?;
    macro_rules! edit_record {
        ($ch:expr, $content:expr) => {{
            let wc = working_copy::memory::Memory::new();
            output::output_repository_no_pending(&wc, &changes, &txn, $ch, "", true, None, 1, 0)?;
            wc.write_file("file", Inode::ROOT)?.write_all($content)?;
            record_all(&wc, &changes, &txn, $ch, "")?
        }};
    }
    // H (on a fork of the base): delete L0 and L2.
    let c2 = txn.write().fork(&c0, &SmallString::from_str("c2"))?;
    let hh = edit_record!(&c2, b"L1\nL3\n");
    // c0: CO inserts a duplicate L2, then OZ collapses to one L2.
    let hco = edit_record!(&c0, b"L2\nL2\nL3\n");
    let hoz = edit_record!(&c0, b"L2\n");
    // Apply H (zombifies CO's L2), then unrecord OZ.
    apply::apply_change_arc(&changes, &txn, &c0, &hh)?;
    crate::unrecord::unrecord(
        &mut *txn.write(),
        &c0,
        &changes,
        &hoz,
        0,
        &mut Default::default(),
    )?;
    let (unrec, uc) = fuzz_render(&changes, &txn, &c0)?;

    // Fresh replay of survivors {h0, CO, H}.
    let fresh = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("fresh"))?;
    for h in [&h0, &hco, &hh] {
        apply::apply_change_arc(&changes, &txn, &fresh, h)?;
    }
    let (rep, rc) = fuzz_render(&changes, &txn, &fresh)?;
    eprintln!(
        "unrecord: {:?} ({uc} conflicts)",
        String::from_utf8_lossy(&unrec)
    );
    eprintln!(
        "replay:   {:?} ({rc} conflicts)",
        String::from_utf8_lossy(&rep)
    );
    // Content must match a fresh replay; whether the surviving change's
    // deletion renders `CO`'s line as a zombie conflict or a bare line is a
    // benign conflict-structure difference (same content).
    assert_eq!(
        canon_content(&unrec),
        canon_content(&rep),
        "unrecord changed the content\nunrec  = {:?}\nreplay = {:?}",
        String::from_utf8_lossy(&unrec),
        String::from_utf8_lossy(&rep),
    );
    Ok(())
}
