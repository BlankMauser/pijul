//! The parallel record walk (`Builder::record` with `n_workers > 1`) must be
//! *deterministic*: it prepares each file's diff concurrently but commits the
//! results single-threaded in walk order, so the change it produces is
//! byte-for-byte identical to the single-threaded path regardless of how the
//! worker threads are scheduled. See `notes-record-stat-cache.md` §5.

use super::*;
use crate::record::Builder;
use crate::working_copy::WorkingCopy;
use std::io::Write;

/// Record the current (modified) working copy against `channel` with the given
/// number of workers, without applying, and return the resulting `(contents,
/// actions)`. `force_rediff` is set so every file is diffed (independent of the
/// stat cache), which is exactly the code path we want to compare.
fn record_with_workers<T, R, P>(
    repo: &R,
    changes: &P,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    n_workers: usize,
) -> (
    Vec<u8>,
    Vec<crate::change::Hunk<Option<ChangeId>, crate::change::LocalByte>>,
)
where
    T: MutTxnT + Send + Sync + 'static,
    R: WorkingCopy + Clone + Send + Sync + 'static,
    P: ChangeStore + Clone + Send + 'static,
    R::Error: Send + Sync + 'static,
{
    let mut builder = Builder::new();
    builder.force_rediff = true;
    builder
        .record(
            txn.clone(),
            Algorithm::default(),
            false,
            &crate::DEFAULT_SEPARATOR,
            channel.clone(),
            repo,
            changes,
            "",
            n_workers,
        )
        .unwrap();
    let rec = builder.finish();
    let contents = rec.contents.lock().clone();
    (contents, rec.actions)
}

#[test]
fn parallel_record_is_deterministic() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();

    // A handful of files with multi-line content, so the walk produces several
    // independent diff tasks.
    const N_FILES: usize = 12;
    for i in 0..N_FILES {
        let mut body = String::new();
        for j in 0..40 {
            body.push_str(&format!("file {i} line {j}\n"));
        }
        repo.add_file(&format!("dir/file{i}"), body.into_bytes());
    }

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    for i in 0..N_FILES {
        txn.write().add_file(&format!("dir/file{i}"), 0).unwrap();
    }
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();
    // Initial record + apply, so the files exist in the pristine and later
    // re-records go through the existing-file (parallel diff) path.
    record_all(&repo, &changes, &txn, &channel, "").unwrap();

    // Modify every file: insert, delete and edit lines so each diff is
    // non-trivial.
    for i in 0..N_FILES {
        let mut w = repo
            .write_file(&format!("dir/file{i}"), crate::Inode::ROOT)
            .unwrap();
        for j in 0..40 {
            if j % 7 == 0 {
                writeln!(w, "file {i} INSERTED before {j}").unwrap();
            }
            if j % 5 != 0 {
                writeln!(w, "file {i} line {j} EDITED").unwrap();
            }
        }
    }

    // Record the same state single-threaded and with 8 workers. Neither record
    // applies, so both see an identical pristine and working copy.
    let (contents_seq, actions_seq) = record_with_workers(&repo, &changes, &txn, &channel, 1);
    let (contents_par, actions_par) = record_with_workers(&repo, &changes, &txn, &channel, 8);

    assert!(!actions_seq.is_empty(), "expected a non-empty change");
    assert_eq!(
        contents_seq, contents_par,
        "parallel record produced a different contents blob"
    );
    assert_eq!(
        actions_seq, actions_par,
        "parallel record produced different actions"
    );
    Ok(())
}

/// §6 directory-mtime pruning must never *miss* a deletion: once a directory's
/// mtime has been cached, removing a file from it has to bump that mtime so the
/// next record rescans the directory and records the deletion. This exercises
/// the warm-cache path that the existing `rm_file` tests don't.
#[test]
fn dir_pruning_still_detects_deletion() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    for f in ["d/a", "d/b", "d/c"] {
        repo.add_file(f, b"x\ny\nz\n".to_vec());
    }

    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    for f in ["d/a", "d/b", "d/c"] {
        txn.write().add_file(f, 0).unwrap();
    }
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))
        .unwrap();

    // 1. Initial record: creates `d` (a *new* directory, so it never goes
    //    through the pruning path).
    record_all(&repo, &changes, &txn, &channel, "").unwrap();

    // 2. Edit a file and record again: now `d` exists in the pristine, so this
    //    record caches `d`'s mtime via `dir_entries_unchanged`.
    {
        let mut w = repo.write_file("d/a", crate::Inode::ROOT).unwrap();
        w.write_all(b"x\ny CHANGED\nz\n").unwrap();
    }
    record_all(&repo, &changes, &txn, &channel, "").unwrap();
    assert!(txn.read().is_tracked("d/b").unwrap());

    // 3. Remove d/b. This must bump `d`'s mtime (POSIX / memory WC), so the
    //    warm cache does NOT cause the deletion to be skipped.
    repo.remove_path("d/b", false)?;
    record_all(&repo, &changes, &txn, &channel, "").unwrap();

    assert!(
        !txn.read().is_tracked("d/b").unwrap(),
        "deletion of d/b was missed — directory-mtime pruning skipped a changed directory"
    );
    assert!(txn.read().is_tracked("d/a").unwrap());
    assert!(txn.read().is_tracked("d/c").unwrap());
    Ok(())
}
