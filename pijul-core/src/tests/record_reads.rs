//! Regression tests for the mtime optimisation in `record`.
//!
//! `record` walks the whole tree, but it must only *read* (and diff) files
//! whose modification time is newer than the channel's last-modified stamp
//! (see `modified_since_last_commit` in `crate::record`). Reading a file that
//! has not changed since the last commit is pure waste — on a large working
//! copy it turns a one-file commit into a full-tree re-diff (the pathological
//! case that motivated these tests).
//!
//! To pin the invariant down we wrap the in-memory working copy in
//! [`Guarded`], which **panics** if `record` calls `read_file` on a path that
//! the test did not explicitly mark as changed. `file_metadata` and
//! `modified_time` (the cheap probes `record` is *supposed* to use to decide
//! what to read) are always allowed.

use super::*;
use crate::working_copy::{WorkingCopy, WorkingCopyRead, memory};
use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

/// A working copy that fails the test if `record` reads a file it should have
/// skipped thanks to the mtime optimisation.
#[derive(Clone)]
struct Guarded {
    inner: memory::Memory,
    state: Arc<Mutex<GuardState>>,
}

#[derive(Default)]
struct GuardState {
    /// When `false`, reads are unrestricted (used while setting the scene).
    armed: bool,
    /// Paths that are legitimately allowed to be read while armed.
    allowed: HashSet<String>,
    /// Paths actually read while armed (for positive assertions).
    read: HashSet<String>,
}

impl Guarded {
    fn new() -> Self {
        Guarded {
            inner: memory::Memory::new(),
            state: Arc::new(Mutex::new(GuardState::default())),
        }
    }

    /// Arm the guard: from now on, reading any path not in `allowed` panics.
    fn arm<I: IntoIterator<Item = &'static str>>(&self, allowed: I) {
        let mut st = self.state.lock();
        st.armed = true;
        st.allowed = allowed.into_iter().map(|s| s.to_string()).collect();
        st.read.clear();
    }

    /// Disarm and return the set of paths that were read while armed.
    fn disarm(&self) -> HashSet<String> {
        let mut st = self.state.lock();
        st.armed = false;
        std::mem::take(&mut st.read)
    }
}

impl WorkingCopyRead for Guarded {
    type Error = memory::Error;
    fn file_metadata(&self, file: &str) -> Result<InodeMetadata, Self::Error> {
        self.inner.file_metadata(file)
    }
    fn read_file(&self, file: &str, buffer: &mut Vec<u8>) -> Result<(), Self::Error> {
        {
            let mut st = self.state.lock();
            if st.armed {
                assert!(
                    st.allowed.contains(file),
                    "record read unchanged file {:?}: its mtime is older than the \
                     channel's last-modified stamp, so it should have been skipped",
                    file
                );
                st.read.insert(file.to_string());
            }
        }
        self.inner.read_file(file, buffer)
    }
    fn modified_time(&self, file: &str) -> Result<SystemTime, Self::Error> {
        self.inner.modified_time(file)
    }
    fn file_size(&self, file: &str) -> Result<u64, Self::Error> {
        // A cheap stat, not a content read — always allowed.
        self.inner.file_size(file)
    }
}

impl WorkingCopy for Guarded {
    fn create_dir_all(&self, path: &str) -> Result<(), Self::Error> {
        self.inner.create_dir_all(path)
    }
    fn remove_path(&self, name: &str, rec: bool) -> Result<(), Self::Error> {
        self.inner.remove_path(name, rec)
    }
    fn rename(&self, former: &str, new: &str) -> Result<(), Self::Error> {
        self.inner.rename(former, new)
    }
    fn set_permissions(&self, name: &str, permissions: u16) -> Result<(), Self::Error> {
        self.inner.set_permissions(name, permissions)
    }
    type Writer = memory::Writer;
    fn write_file(&self, file: &str, inode: Inode) -> Result<Self::Writer, Self::Error> {
        self.inner.write_file(file, inode)
    }
    fn touch(&self, name: &str, time: SystemTime) -> Result<(), Self::Error> {
        self.inner.touch(name, time)
    }
}

/// `SystemTime` `secs` seconds after the epoch (a fixed instant far in the past,
/// so any file stamped with it is unambiguously older than the record walk and
/// gets cached).
fn at_secs(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// Pin every file's mtime to a fixed past instant, so the first `record`
/// populates the per-inode stat cache with a non-ambiguous value.
fn pin_past(repo: &Guarded, files: &[&str]) -> Result<(), anyhow::Error> {
    for f in files {
        repo.touch(f, at_secs(100_000))?;
    }
    Ok(())
}

/// A read-only `pijul diff`-style walk: run the record walk but *don't* apply
/// anything, then warm the stat cache from the files it confirmed clean
/// (`clean_only = true`). Mirrors `pijul/src/commands/diff.rs`.
fn diff_warm<T, R, P>(
    repo: &R,
    store: &P,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
) -> Result<(), anyhow::Error>
where
    T: MutTxnT + Send + Sync + 'static,
    R: WorkingCopy + Clone + Send + Sync + 'static,
    P: ChangeStore + Clone + Send + 'static,
    R::Error: Send + Sync + 'static,
{
    let mut state = Builder::new();
    state.record(
        txn.clone(),
        Algorithm::default(),
        false,
        &crate::DEFAULT_SEPARATOR,
        channel.clone(),
        repo,
        store,
        "",
        1,
    )?;
    let rec = state.finish();
    let stat_updates = rec.take_stat_updates();
    crate::record::update_stat_cache(&mut *txn.write(), &stat_updates, true)?;
    Ok(())
}

/// A no-op second `record` (nothing changed on disk) must not read a single
/// file: the stat cache populated by the first record makes every file skip.
#[test]
fn record_skips_unmodified_files() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = Guarded::new();
    let changes = changestore::memory::Memory::new();
    let files = ["a", "b", "c/d", "c/e"];
    for f in files {
        repo.inner.add_file(f, b"one\ntwo\nthree\n".to_vec());
    }

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    for f in files {
        txn.write().add_file(f, 0)?;
    }
    pin_past(&repo, &files)?;
    // First record reads everything and caches each file's (mtime, size).
    record_all(&repo, &changes, &txn, &channel, "")?;

    // Nothing changed on disk: the second record must read nothing.
    repo.arm([]);
    let (_, change) = record_all_change(&repo, &changes, &txn, &channel, "")?;
    let read = repo.disarm();
    assert!(read.is_empty(), "expected no reads, got {:?}", read);
    assert!(
        change.changes.is_empty(),
        "expected an empty change, got {:?}",
        change.changes
    );
    Ok(())
}

/// A read-only `pijul diff` warms the stat cache: after outputting a fresh
/// checkout (cold cache), a *diff* — which never applies anything — makes the
/// next record read nothing. This is what stops repeated `pijul diff` from
/// re-diffing the whole tree every time.
#[test]
fn diff_warms_cache_so_next_walk_reads_nothing() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let source = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    let files = ["a", "b", "c/d", "c/e"];
    for f in files {
        source.add_file(f, b"one\ntwo\nthree\n".to_vec());
    }

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    for f in files {
        txn.write().add_file(f, 0)?;
    }
    record_all(&source, &changes, &txn, &channel, "")?;

    // Fresh checkout into a cold-cache working copy (as clone/pull do).
    let dest = Guarded::new();
    crate::output::output_repository_no_pending(
        &dest, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    pin_past(&dest, &files)?;

    // A `pijul diff` (read-only, no apply) warms the cache for every clean file.
    diff_warm(&dest, &changes, &txn, &channel)?;

    // The next record now reads nothing, even though nothing was recorded before.
    dest.arm([]);
    let (_, change) = record_all_change(&dest, &changes, &txn, &channel, "")?;
    let read = dest.disarm();
    assert!(
        read.is_empty(),
        "record re-diffed files a prior `diff` already confirmed clean: {:?}",
        read
    );
    assert!(
        change.changes.is_empty(),
        "expected empty change, got {:?}",
        change.changes
    );
    Ok(())
}

/// Safety: a `pijul diff` must **never** cache a file that actually differs from
/// the pristine — otherwise the pending edit would be hidden from the next
/// record. Here `b` is edited in place *and* given a backdated mtime (as
/// `tar`/`rsync`/`cp -p` would); the diff walk sees it is dirty, refuses to
/// cache it, and the subsequent record still records the edit.
#[test]
fn diff_never_hides_a_dirty_file() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = Guarded::new();
    let changes = changestore::memory::Memory::new();
    let files = ["a", "b"];
    for f in files {
        repo.inner.add_file(f, b"one\ntwo\nthree\n".to_vec());
    }

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    for f in files {
        txn.write().add_file(f, 0)?;
    }
    pin_past(&repo, &files)?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    // Edit `b` in place, then backdate its mtime *below* the cached one — the
    // case a channel-wide mtime stamp could not catch.
    repo.write_file("b", Inode::ROOT)?
        .write_all(b"one\ntwo\nthree\nfour\n")?;
    repo.touch("b", at_secs(50_000))?;

    // A diff over the dirty tree must not cache `b` as clean.
    diff_warm(&repo, &changes, &txn, &channel)?;

    // The record after the diff must still see and record `b`'s edit.
    let (_, change) = record_all_change(&repo, &changes, &txn, &channel, "")?;
    assert_eq!(
        change.changes.len(),
        1,
        "the diff hid the pending edit to `b`: {:?}",
        change.changes
    );
    Ok(())
}

/// When a single file changes, `record` must read that file and *only* that
/// file — never the untouched neighbours (which the stat cache skips).
#[test]
fn record_reads_only_modified_file() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = Guarded::new();
    let changes = changestore::memory::Memory::new();
    let files = ["a", "b", "c/d", "c/e"];
    for f in files {
        repo.inner.add_file(f, b"one\ntwo\nthree\n".to_vec());
    }

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    for f in files {
        txn.write().add_file(f, 0)?;
    }
    pin_past(&repo, &files)?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    // Change exactly one file. Give it a *different* (still past, so
    // unambiguous) mtime so it no longer matches its cached stat.
    repo.write_file("c/d", Inode::ROOT)?
        .write_all(b"one\ntwo\nthree\nfour\n")?;
    repo.touch("c/d", at_secs(200_000))?;

    repo.arm(["c/d"]);
    let (_, change) = record_all_change(&repo, &changes, &txn, &channel, "")?;
    let read = repo.disarm();

    assert!(read.contains("c/d"), "the modified file was not read");
    assert_eq!(
        read.len(),
        1,
        "record read unrelated files as well: {:?}",
        read
    );
    assert_eq!(
        change.changes.len(),
        1,
        "expected exactly one hunk, got {:?}",
        change.changes
    );
    Ok(())
}

/// Moving a file locally (its content unchanged, only its parent/name) must not
/// invalidate the stat cache — the next record should read nothing. A local move
/// never re-`put_inodes` the file (apply doesn't remap the inode), so the cached
/// `(mtime, size)` simply carries over; see `pulled_move_preserves_stat_cache`
/// for the `output`-side path where `put_inodes` *is* called.
#[test]
fn local_move_preserves_stat_cache() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = Guarded::new();
    let changes = changestore::memory::Memory::new();
    let files = ["a", "b", "c/d", "c/e"];
    for f in files {
        repo.inner.add_file(f, b"one\ntwo\nthree\n".to_vec());
    }

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    for f in files {
        txn.write().add_file(f, 0)?;
    }
    pin_past(&repo, &files)?;
    // Warm the cache.
    record_all(&repo, &changes, &txn, &channel, "")?;

    // Move `a` -> `a2`, content untouched (a real move preserves the file's
    // bytes and mtime — model that by re-pinning the destination to the same
    // past instant).
    txn.write().move_file("a", "a2", 0)?;
    repo.rename("a", "a2")?;
    repo.touch("a2", at_secs(100_000))?;

    // Record the move (this is where the move hunk is emitted) …
    record_all(&repo, &changes, &txn, &channel, "")?;

    // … and now a no-op record must read nothing — the moved file's stat is
    // still valid.
    repo.arm(["a", "a2", "b", "c/d", "c/e"]);
    let _ = record_all_change(&repo, &changes, &txn, &channel, "")?;
    let read = repo.disarm();
    assert!(
        read.is_empty(),
        "a move re-diffed files (stat cache lost on move): {:?}",
        read
    );
    Ok(())
}

/// A *pulled* move must preserve the stat cache. Bob has a warm cache, then pulls
/// Alice's move and runs `output`, which renames Bob's working-copy file and
/// re-`put_inodes` it with the *same* inode-vertex position (a move changes only
/// the FOLDER/name edges, never the inode vertex). `put_inodes` keeps the cached
/// `(mtime, size)` precisely because the position is unchanged, so the next
/// record reads nothing. Disabling that preservation branch makes this test fail
/// (the moved file gets needlessly re-diffed).
#[test]
fn pulled_move_preserves_stat_cache() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    // Alice authors: create files, then move `a` -> `a2`.
    let alice = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    let files = ["a", "b", "c/d", "c/e"];
    for f in files {
        alice.add_file(f, b"one\ntwo\nthree\n".to_vec());
    }
    let env_a = pristine::sanakirja::Pristine::new_anon()?;
    let txn_a = env_a.arc_txn_begin().unwrap();
    let ch_a = txn_a
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    for f in files {
        txn_a.write().add_file(f, 0)?;
    }
    let a0 = record_all(&alice, &changes, &txn_a, &ch_a, "")?;
    txn_a.write().move_file("a", "a2", 0)?;
    alice.rename("a", "a2")?;
    let a1 = record_all(&alice, &changes, &txn_a, &ch_a, "")?;

    // Bob clones a0, warms his cache, then pulls a1 and outputs.
    let bob = Guarded::new();
    let env_b = pristine::sanakirja::Pristine::new_anon()?;
    let txn_b = env_b.arc_txn_begin().unwrap();
    let ch_b = txn_b
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    txn_b
        .write()
        .apply_change(&changes, &mut *ch_b.write(), &a0)?;
    crate::output::output_repository_no_pending(
        &bob, &changes, &txn_b, &ch_b, "", true, None, 1, 0,
    )?;
    pin_past(&bob, &files)?;
    record_all(&bob, &changes, &txn_b, &ch_b, "")?; // warm

    // Pull the move and materialise it (this renames `a` -> `a2` on disk).
    txn_b
        .write()
        .apply_change(&changes, &mut *ch_b.write(), &a1)?;
    crate::output::output_repository_no_pending(
        &bob, &changes, &txn_b, &ch_b, "", true, None, 1, 0,
    )?;
    // A real move preserves the file's bytes/mtime; model that.
    pin_past(&bob, &["a2", "b", "c/d", "c/e"])?;

    // The record after the pull must read nothing.
    bob.arm(["a", "a2", "b", "c/d", "c/e"]);
    let _ = record_all_change(&bob, &changes, &txn_b, &ch_b, "")?;
    let read = bob.disarm();
    assert!(
        read.is_empty(),
        "a pulled move re-diffed files (stat lost in output's put_inodes): {:?}",
        read
    );
    Ok(())
}

/// After materialising the working copy with `output` (as `clone`/`pull`/`reset`
/// do), the *first* record populates the stat cache and a *second* record reads
/// nothing — even though `output` wrote every file with a fresh mtime. This is
/// what stops the pathological full-tree re-diff after a pull.
#[test]
fn output_then_record_twice_reads_nothing() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let source = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    let files = ["a", "b", "c/d", "c/e"];
    for f in files {
        source.add_file(f, b"one\ntwo\nthree\n".to_vec());
    }

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    for f in files {
        txn.write().add_file(f, 0)?;
    }
    record_all(&source, &changes, &txn, &channel, "")?;

    // Simulate `clone`/`pull`: output the whole repository into a fresh working
    // copy (files get whatever mtime `output` writes).
    let dest = Guarded::new();
    crate::output::output_repository_no_pending(
        &dest, &changes, &txn, &channel, "", true, None, 1, 0,
    )?;
    // Pin the just-written files to a past instant so the first record caches
    // them unambiguously (mirrors a checkout whose files predate the record).
    pin_past(&dest, &files)?;

    // First record after checkout: reads everything, populates the cache.
    record_all(&dest, &changes, &txn, &channel, "")?;

    // Second record: nothing changed → reads nothing.
    dest.arm([]);
    let (_, change) = record_all_change(&dest, &changes, &txn, &channel, "")?;
    let read = dest.disarm();
    assert!(
        read.is_empty(),
        "record re-diffed unchanged files after a checkout: {:?}",
        read
    );
    assert!(
        change.changes.is_empty(),
        "expected an empty change, got {:?}",
        change.changes
    );
    Ok(())
}
