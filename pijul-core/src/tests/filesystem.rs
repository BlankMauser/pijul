use super::*;
use std::io::Write;

const MAX_FILES: usize = 10;

/// `reset` outputs with `if_modified_since` = the channel's last modification.
/// An edit whose mtime predates that (restored by `cp -p`, `tar`, `rsync -a`…)
/// must still be reverted, not skipped on the strength of its old mtime.
#[test]
fn output_if_modified_since_old_mtime_edit() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let r = tempfile::tempdir()?;
    let repo = working_copy::filesystem::FileSystem::from_root(r.path());
    let f = tempfile::tempdir()?;
    let changes = changestore::filesystem::FileSystem::from_root(f.path(), MAX_FILES);

    repo.write_file("file", Inode::ROOT)
        .unwrap()
        .write_all(&b"a\n"[..])
        .unwrap();

    let f = tempfile::tempdir()?;
    let env = pristine::sanakirja::Pristine::new(f.path().join("pristine"))?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("file", 0).unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();
    record_all(&repo, &changes, &txn, &channel, "").unwrap();

    // Same size, but an mtime well before the channel's last modification.
    let path = r.path().join("file");
    std::fs::write(&path, b"z\n")?;
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::open(&path)?.set_times(std::fs::FileTimes::new().set_modified(old))?;

    let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    output::output_repository_no_pending(
        &repo,
        &changes,
        &txn,
        &channel,
        "",
        true,
        Some(since),
        1,
        0,
    )
    .unwrap();
    assert_eq!(std::fs::read(&path)?, b"a\n");
    Ok(())
}

/// Output `channel` as the current channel, stamping the stat cache.
fn stamp<T: MutTxnT + Send + Sync + 'static>(
    repo: &working_copy::filesystem::FileSystem,
    changes: &changestore::filesystem::FileSystem,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
) -> Result<(), anyhow::Error> {
    output::output_repository_no_pending(repo, changes, txn, channel, "", true, None, 1, 0)?;
    Ok(())
}

/// What `reset` does: skip the files whose mtime predates `since`, if the stat
/// cache says they match the pristine. `since` is in the future, so this only
/// renders the files the cache does not vouch for.
fn reset_trusting_cache<T: MutTxnT + Send + Sync + 'static>(
    repo: &working_copy::filesystem::FileSystem,
    changes: &changestore::filesystem::FileSystem,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
) -> Result<(), anyhow::Error> {
    let since = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    output::output_repository_no_pending(repo, changes, txn, channel, "", true, Some(since), 1, 0)?;
    Ok(())
}

/// A file at "a\n", then a change `h` turning it into "b\n", unrecorded again:
/// the pristine and the working copy are at "a\n", stamped clean.
fn a_then_b() -> Result<
    (
        tempfile::TempDir,
        working_copy::filesystem::FileSystem,
        tempfile::TempDir,
        changestore::filesystem::FileSystem,
        tempfile::TempDir,
        ArcTxn<pristine::sanakirja::MutTxn0>,
        ChannelRef<pristine::sanakirja::MutTxn0>,
        Hash,
    ),
    anyhow::Error,
> {
    let r = tempfile::tempdir()?;
    let repo = working_copy::filesystem::FileSystem::from_root(r.path());
    let c = tempfile::tempdir()?;
    let changes = changestore::filesystem::FileSystem::from_root(c.path(), MAX_FILES);
    let p = tempfile::tempdir()?;
    let env = pristine::sanakirja::Pristine::new(p.path().join("pristine"))?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();
    let path = r.path().join("file");
    std::fs::write(&path, b"a\n")?;
    txn.write().add_file("file", 0)?;
    record_all(&repo, &changes, &txn, &channel, "")?;
    std::fs::write(&path, b"b\n")?;
    let h = record_all(&repo, &changes, &txn, &channel, "")?;
    txn.write()
        .unrecord(&changes, &channel, &h, 0, &mut Default::default())?;
    std::fs::write(&path, b"a\n")?;
    stamp(&repo, &changes, &txn, &channel)?;
    Ok((r, repo, c, changes, p, txn, channel, h))
}

/// Applying a change without outputting the files it touches (a dependency
/// `pijul apply` pulled in, `--deps-only`, `replay`…) leaves them stale on disk:
/// their stat must stop vouching for them, or `record`/`diff` see nothing and
/// `reset` skips them.
#[test]
fn apply_without_output_forgets_stat() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let (r, repo, _c, changes, _p, txn, channel, h) = a_then_b()?;
    let inode = crate::fs::find_inode(&*txn.read(), "file")?;
    assert!(txn.read().get_inode_stat(&inode)?.is_some());

    txn.write()
        .apply_change(&changes, &mut *channel.write(), &h)?;
    assert_eq!(txn.read().get_inode_stat(&inode)?, None);

    reset_trusting_cache(&repo, &changes, &txn, &channel)?;
    assert_eq!(std::fs::read(r.path().join("file"))?, b"b\n");
    Ok(())
}

/// Same for an unrecord that keeps the working copy (`pijul unrecord` without
/// `--reset`, or the pending patch of pull/apply after its output).
#[test]
fn unrecord_without_output_forgets_stat() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let (r, repo, _c, changes, _p, txn, channel, h) = a_then_b()?;
    txn.write()
        .apply_change(&changes, &mut *channel.write(), &h)?;
    stamp(&repo, &changes, &txn, &channel)?;
    let inode = crate::fs::find_inode(&*txn.read(), "file")?;
    assert!(txn.read().get_inode_stat(&inode)?.is_some());

    txn.write()
        .unrecord(&changes, &channel, &h, 0, &mut Default::default())?;
    assert_eq!(txn.read().get_inode_stat(&inode)?, None);

    reset_trusting_cache(&repo, &changes, &txn, &channel)?;
    assert_eq!(std::fs::read(r.path().join("file"))?, b"a\n");
    Ok(())
}

/// Only the files a change touches lose their stat: the next `record` re-reads
/// these, not the whole tree.
#[test]
fn apply_keeps_stat_of_untouched_files() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let (_r, repo, _c, changes, _p, txn, channel, _h) = a_then_b()?;
    let inode = crate::fs::find_inode(&*txn.read(), "file")?;
    let before = txn.read().get_inode_stat(&inode)?;
    assert!(before.is_some());
    // A change touching another file leaves this one's stat alone.
    std::fs::write(_r.path().join("other"), b"x\n")?;
    txn.write().add_file("other", 0)?;
    record_all(&repo, &changes, &txn, &channel, "other")?;
    assert_eq!(txn.read().get_inode_stat(&inode)?, before);
    Ok(())
}

#[test]
fn filesystem() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let r = tempfile::tempdir()?;
    let repo = working_copy::filesystem::FileSystem::from_root(r.path());

    let f = tempfile::tempdir()?;
    let changes = changestore::filesystem::FileSystem::from_root(f.path(), MAX_FILES);

    repo.write_file("dir/file", Inode::ROOT)
        .unwrap()
        .write_all(&b"a\nb\nc\nd\ne\nf\n"[..])
        .unwrap();

    let f = tempfile::tempdir()?;
    let env = pristine::sanakirja::Pristine::new(f.path().join("pristine"))?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("dir/file", 0).unwrap();

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();
    let p = record_all(&repo, &changes, &txn, &channel, "").unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main2"))
        .unwrap();
    info!("applying");
    apply::apply_change_arc(&changes, &txn, &channel, &p)?;
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)
        .unwrap();

    txn.commit().unwrap();

    repo.rename("dir/file", "dir/file.old")?;
    repo.remove_path("dir/file.old", false)?;
    repo.remove_path("dir", true)?;
    Ok(())
}

#[test]
fn symlink() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let r = tempfile::tempdir()?;
    let repo = working_copy::filesystem::FileSystem::from_root(r.path());

    let f = tempfile::tempdir()?;
    let changes = changestore::filesystem::FileSystem::from_root(f.path(), MAX_FILES);

    repo.write_file("dir/file", Inode::ROOT)
        .unwrap()
        .write_all(&b"a\nb\nc\nd\ne\nf\n"[..])
        .unwrap();
    std::os::unix::fs::symlink(&r.path().join("dir/file"), &r.path().join("dir/link")).unwrap();

    let f = tempfile::tempdir()?;
    std::fs::create_dir_all(f.path())?;
    let env = pristine::sanakirja::Pristine::new(f.path().join("pristine"))?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("dir/file", 0).unwrap();
    txn.write().add_file("dir/link", 0).unwrap();

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();
    let p = record_all(&repo, &changes, &txn, &channel, "").unwrap();
    info!("applying");
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main2"))
        .unwrap();
    apply::apply_change_arc(&changes, &txn, &channel, &p)?;
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)
        .unwrap();

    txn.commit().unwrap();

    repo.rename("dir/file", "dir/file.old")?;
    repo.remove_path("dir/file.old", false)?;
    repo.remove_path("dir", true)?;
    Ok(())
}

#[test]
fn record_dead_symlink() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let r = tempfile::tempdir()?;
    let repo = working_copy::filesystem::FileSystem::from_root(r.path());

    let f = tempfile::tempdir()?;
    let changes = changestore::filesystem::FileSystem::from_root(f.path(), MAX_FILES);

    std::fs::create_dir_all(&r.path().join("dir")).unwrap();
    std::os::unix::fs::symlink("../file", &r.path().join("dir/link")).unwrap();

    let f = tempfile::tempdir()?;
    std::fs::create_dir_all(f.path())?;
    let env = pristine::sanakirja::Pristine::new(f.path().join("pristine"))?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("dir/link", 0).unwrap();

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();
    let p = record_all(&repo, &changes, &txn, &channel, "").unwrap();
    info!("applying");
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main2"))
        .unwrap();
    apply::apply_change_arc(&changes, &txn, &channel, &p)?;
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)
        .unwrap();

    txn.commit().unwrap();
    Ok(())
}

#[test]
fn overwrite_dead_symlink() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let r = tempfile::tempdir()?;
    let repo = working_copy::filesystem::FileSystem::from_root(r.path());

    let f = tempfile::tempdir()?;
    let changes = changestore::filesystem::FileSystem::from_root(f.path(), MAX_FILES);

    repo.write_file("dir/file", Inode::ROOT)
        .unwrap()
        .write_all(&b"a\nb\nc\nd\ne\nf\n"[..])
        .unwrap();

    let f = tempfile::tempdir()?;
    std::fs::create_dir_all(f.path())?;
    let env = pristine::sanakirja::Pristine::new(f.path().join("pristine"))?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("dir/file", 0).unwrap();

    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();
    let p = record_all(&repo, &changes, &txn, &channel, "").unwrap();
    info!("applying");
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main2"))
        .unwrap();

    // Substitute dir/file with a dead symlink
    std::fs::remove_file(&r.path().join("dir/file")).unwrap();
    std::os::unix::fs::symlink("a/b/c/d/file", &r.path().join("dir/file")).unwrap();
    debug!("meta = {:?}", std::fs::metadata("dir/file"));
    // And output.
    apply::apply_change_arc(&changes, &txn, &channel, &p)?;
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)
        .unwrap();

    txn.commit().unwrap();
    Ok(())
}

/// A change `h` recorded for a file with contents, as stored on disk (`full`),
/// and the same file cut right before its contents section (`metadata_only`).
fn full_and_metadata_only() -> Result<(Hash, Vec<u8>, Vec<u8>), anyhow::Error> {
    let r = tempfile::tempdir()?;
    let repo = working_copy::filesystem::FileSystem::from_root(r.path());
    let c = tempfile::tempdir()?;
    let changes = changestore::filesystem::FileSystem::from_root(c.path(), MAX_FILES);
    let p = tempfile::tempdir()?;
    let env = pristine::sanakirja::Pristine::new(p.path().join("pristine"))?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();
    std::fs::write(r.path().join("file"), b"some contents\n")?;
    txn.write().add_file("file", 0)?;
    let h = record_all(&repo, &changes, &txn, &channel, "")?;
    let full = std::fs::read(changes.filename(&h))?;
    let offsets: crate::change::Offsets =
        bincode::deserialize(&full[..crate::change::Change::OFFSETS_SIZE as usize])?;
    let metadata_only = full[..offsets.contents_off as usize].to_vec();
    assert!(metadata_only.len() < full.len());
    Ok((h, full, metadata_only))
}

/// A reader that opened the old, metadata-only file of a change must not
/// cache that handle once a writer has replaced the file and invalidated the
/// entry: later reads of the contents would keep failing.
#[test]
fn replacement_during_load_is_not_cached() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let (h, full, metadata_only) = full_and_metadata_only()?;
    let dir = tempfile::tempdir()?;
    let store = changestore::filesystem::FileSystem::from_root(dir.path(), MAX_FILES);
    let id = ChangeId(L64(7));
    store.save_from_buf_unchecked(&metadata_only, &h, Some(id))?;
    assert!(!store.has_contents(h, Some(id)));

    // Reader opens the metadata-only file; the writer installs the complete
    // one and invalidates; only then does the reader get to insert.
    let old = store.load_with(
        |_| Some(h),
        id,
        || store.save_from_buf_unchecked(&full, &h, Some(id)).unwrap(),
    )?;
    // The reader notices the replacement and re-opens the current file...
    assert!(old.lock().has_contents());
    // ...and nobody is handed the stale handle from the cache either.
    assert!(store.has_contents(h, Some(id)));
    let again = store.load(|_| Some(h), id)?;
    assert!(again.lock().has_contents());
    Ok(())
}

/// The same race with real threads: whatever the interleaving, once the writer
/// has returned, readers see the complete file.
#[test]
fn concurrent_replacement_converges() -> Result<(), anyhow::Error> {
    let (h, full, metadata_only) = full_and_metadata_only()?;
    for round in 0..50u64 {
        let dir = tempfile::tempdir()?;
        let store = changestore::filesystem::FileSystem::from_root(dir.path(), MAX_FILES);
        let id = ChangeId(L64(round + 1));
        store.save_from_buf_unchecked(&metadata_only, &h, Some(id))?;
        std::thread::scope(|s| {
            for _ in 0..3 {
                let store = store.clone();
                s.spawn(move || {
                    for _ in 0..20 {
                        let _ = store.load(|_| Some(h), id);
                    }
                });
            }
            store.save_from_buf_unchecked(&full, &h, Some(id)).unwrap();
        });
        assert!(store.has_contents(h, Some(id)), "round {round}");
        assert!(store.load(|_| Some(h), id)?.lock().has_contents());
    }
    Ok(())
}
