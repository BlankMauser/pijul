//! Tests for the monorepo sub-root / boundary machinery.
//!
//! This file also hosts the unit tests for [`crate::record::boundary_of`], the
//! pure predicate behind the `record` boundary-crossing guard. The wider
//! sub-root integration tests (`clone --into`, `relocate_sub_root`,
//! `group_by_sub_root`) live on `main`; this snapshot carries the boundary
//! coverage added with the shared-boundaries feature.

use super::*;
use crate::record::boundary_of;
use std::io::Write;

/// Record the whole working copy of `repo` (prefix ""), and return the number of
/// distinct sub-roots the resulting change touches (via
/// [`crate::record::group_by_sub_root`]), then apply it so the pristine stays in
/// sync. This mirrors the CLI's cross-root guard, which bails when the count > 1.
fn record_and_count_sub_roots<
    T: MutTxnT + Send + Sync + 'static,
    R: WorkingCopy + Clone + Send + Sync + 'static,
    P: ChangeStore + Clone + Send + 'static,
>(
    repo: &R,
    store: &P,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
) -> Result<usize, anyhow::Error>
where
    R::Error: Send + Sync + 'static,
{
    use crate::record::{Algorithm, Builder};
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
    let changes: Vec<_> = rec
        .actions
        .into_iter()
        .map(|r| r.globalize(&*txn.read()).unwrap())
        .collect();
    let mut change0 = crate::change::Change::make_change(
        &*txn.read(),
        channel,
        changes,
        std::mem::take(&mut *rec.contents.lock()),
        crate::change::ChangeHeader {
            message: "test".to_string(),
            authors: vec![],
            description: None,
            timestamp: jiff::Timestamp::now(),
        },
        Vec::new(),
    )
    .unwrap();
    let groups = {
        let txn_ = txn.read();
        crate::record::group_by_sub_root(
            &*txn_,
            &*channel.read(),
            &change0.hashed.changes,
            &rec.updatables,
        )?
    };
    let hash = store.save_change(&mut change0, |_, _| Ok::<_, anyhow::Error>(()))?;
    apply::apply_local_change(&mut *txn.write(), channel, &change0, &hash, &rec.updatables)?;
    Ok(groups.len())
}

/// Changing several files of a *single* repository — even every file, across
/// more than one directory level — must never look like a cross-root record:
/// all the files live under the one root, so [`group_by_sub_root`] must return
/// a single group.
///
/// [`group_by_sub_root`]: crate::record::group_by_sub_root
#[test]
fn single_repo_all_files_is_one_sub_root() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let files = ["a/b/c/file1", "a/b/file2", "a/d/file3", "top"];

    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    for p in files {
        repo.add_file(p, b"x\ny\nz\n".to_vec());
    }
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    for p in files {
        txn.write().add_file(p, 0)?;
    }
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;

    // Initial record (creates the single root) — one sub-root.
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );

    // Touch *all* the files at once — still one sub-root, no cross-root guard.
    for p in files {
        repo.write_file(p, Inode::ROOT)?.write_all(b"changed\n")?;
    }
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );
    Ok(())
}

/// Regression test for the false "2 independent roots" reported by `pijul
/// record`. A repository whose single (non-relocated) root was created by an
/// `AddRoot` hunk — as `apply_root_change` mints, and as observed in the field
/// on a repo where an `AddRoot` patch introduced the root — has its existing,
/// already-tracked content resolve (through the *graph*) to that root's INODE,
/// while a brand-new *top-level* file resolves (through the `tree` table) to
/// `Inode::ROOT`. Before the fix these landed in two different groups
/// (`Existing(".")` vs `New`) and a record adding a top-level file alongside an
/// edit was falsely refused as crossing two roots. A non-relocated root is the
/// *main* project, not a sub-root, so both must fold into a single group.
#[test]
fn add_top_level_file_to_existing_root_is_one_sub_root() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    // Start with content nested under a directory so the initial record mints a
    // proper (non-relocated) root, exactly like a normal `pijul init` repo.
    repo.add_file("dir/existing", b"x\ny\n".to_vec());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("dir/existing", 0)?;
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );

    // Now edit the existing file *and* add a new file straight at the top level.
    // The edit resolves via the graph to the root; the new top-level file
    // resolves via the tree to `Inode::ROOT`. Both are the same single project.
    repo.write_file("dir/existing", Inode::ROOT)?
        .write_all(b"x2\n")?;
    repo.add_file("toplevel", b"new\n".to_vec());
    txn.write().add_file("toplevel", 0)?;
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );
    Ok(())
}

/// A repository assembled from two independent repositories genuinely has two
/// sub-roots — but *only once the imported one is relocated* into a
/// subdirectory (a `DELETED | FOLDER | PARENT` edge back to ROOT), which is
/// exactly what `clone --into` does. Two bare roots sitting side by side
/// directly under ROOT are *not* two independent sub-roots: an un-relocated
/// root is just the main project. After relocating B under `vendor/`, a change
/// touching a file in each spans both, so [`group_by_sub_root`] must report two
/// groups — this is what the CLI's cross-root guard refuses.
///
/// [`group_by_sub_root`]: crate::record::group_by_sub_root
#[test]
fn merged_repos_are_two_sub_roots() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let changes = changestore::memory::Memory::new();

    // Repo A.
    let repo = working_copy::memory::Memory::new();
    for p in ["a/x", "a/y"] {
        repo.add_file(p, b"a\nb\n".to_vec());
    }
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    for p in ["a/x", "a/y"] {
        txn.write().add_file(p, 0)?;
    }
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    // Repo B, recorded in its own pristine (its own, distinct root) but sharing
    // the change store so its change can be applied onto A.
    let repo_b = working_copy::memory::Memory::new();
    for p in ["p/q1", "p/q2"] {
        repo_b.add_file(p, b"m\nn\n".to_vec());
    }
    let env_b = pristine::sanakirja::Pristine::new_anon()?;
    let txn_b = env_b.arc_txn_begin().unwrap();
    for p in ["p/q1", "p/q2"] {
        txn_b.write().add_file(p, 0)?;
    }
    let channel_b = txn_b
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let b0 = record_all(&repo_b, &changes, &txn_b, &channel_b, "")?;
    txn_b.commit().unwrap();

    // Snapshot A's own (empty) root NAME vertices before the merge, and pick A's
    // sub-root INODE as the relocation destination parent — mirroring what
    // `clone --into` does.
    let f0 = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
    let f1 = f0 | EdgeFlags::PSEUDO;
    let (pre_names, dest_parent): (
        std::collections::HashSet<Vertex<ChangeId>>,
        Position<ChangeId>,
    ) = {
        let t = txn.read();
        let ch = channel.read();
        let graph = t.graph(&*ch);
        let mut names = std::collections::HashSet::new();
        let mut dest_parent = None;
        for e in iter_adjacent(&*t, graph, Vertex::ROOT, f0, f1)? {
            let e = e?;
            let child = *t.find_block(graph, e.dest()).unwrap();
            if child.start != child.end {
                continue;
            }
            names.insert(child);
            if dest_parent.is_none() {
                if let Some(e2) = iter_adjacent(&*t, graph, child, f0, f1)?.next() {
                    let inode = *t.find_block(graph, e2?.dest()).unwrap();
                    dest_parent = Some(Position {
                        change: inode.change,
                        pos: inode.start,
                    });
                }
            }
        }
        (names, dest_parent.expect("A must already have a sub-root"))
    };

    // Merge B into A.
    apply::apply_change(&changes, &mut *txn.write(), &mut *channel.write(), &b0)?;

    // Two non-relocated roots sitting side by side under ROOT are *not* two
    // independent sub-roots: only a root that has been moved out of ROOT into a
    // subdirectory (a `DELETED | FOLDER | PARENT` edge back to ROOT) counts, as
    // `clone --into` produces. So relocate B's freshly imported root under a
    // `vendor/` directory of A — that is what makes it a genuine sub-root.
    let b_name = {
        let t = txn.read();
        let ch = channel.read();
        let graph = t.graph(&*ch);
        let mut found = None;
        for e in iter_adjacent(&*t, graph, Vertex::ROOT, f0, f1)? {
            let e = e?;
            let child = *t.find_block(graph, e.dest()).unwrap();
            if child.start == child.end && !pre_names.contains(&child) {
                found = Some(child);
                break;
            }
        }
        found.expect("B's imported root NAME vertex")
    };
    let mut reloc = crate::record::relocate_sub_root(
        &*txn.read(),
        &channel,
        dest_parent,
        b_name,
        "vendor",
        crate::change::ChangeHeader {
            message: "relocate B under vendor/".to_string(),
            authors: vec![],
            description: None,
            timestamp: jiff::Timestamp::now(),
        },
    )
    .map_err(|e| anyhow::anyhow!("relocate_sub_root: {:?}", e))?;
    let rh = changes.save_change(&mut reloc, |_, _| Ok::<_, anyhow::Error>(()))?;
    apply::apply_change(&changes, &mut *txn.write(), &mut *channel.write(), &rh)?;

    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)?;
    let mut merged = repo.list_files();
    merged.sort();
    assert!(
        merged.contains(&"a/x".to_string()) && merged.contains(&"vendor/p/q1".to_string()),
        "merged tree: {:?}",
        merged
    );

    // A single change touching A's own file and the relocated sub-root's file
    // spans two sub-roots: A's non-relocated main project (folds into the "main"
    // group) and B's relocated sub-root.
    repo.write_file("a/x", Inode::ROOT)?.write_all(b"a2\n")?;
    repo.write_file("vendor/p/q1", Inode::ROOT)?
        .write_all(b"m2\n")?;
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        2
    );
    Ok(())
}

/// Build a hand-crafted change that adds each of `names` as a top-level file
/// hanging *directly* off [`Vertex::ROOT`], i.e. the legacy "zero-root" layout
/// that predates the multi-root format (no empty `AddRoot` vertex). Recording a
/// fresh repo always produces the multi-root layout, so this is the only way to
/// exercise the zero-root code path.
fn zero_root_add<T: MutTxnT + Send + Sync + 'static>(
    txn: &T,
    channel: &ChannelRef<T>,
    names: &[&str],
) -> Change {
    use crate::change::{Atom, Hunk, LocalByte, NewVertex};
    use crate::changestore::FileMetadata;
    use crate::pristine::InodeMetadata;

    let mut contents = Vec::new();
    let mut hunks: Vec<Hunk<Option<ChangeId>, LocalByte>> = Vec::new();
    for name in names {
        contents.push(0);
        let inode_pos = ChangePosition(contents.len().into());
        contents.push(0);
        let name_start = ChangePosition(contents.len().into());
        FileMetadata {
            metadata: InodeMetadata::new(0o644, false),
            basename: name,
            encoding: None,
        }
        .write(&mut contents);
        let name_end = ChangePosition(contents.len().into());
        contents.push(0);
        hunks.push(Hunk::FileAdd {
            add_name: Atom::NewVertex(NewVertex {
                // Parent is ROOT itself — no intermediate empty root vertex.
                up_context: vec![Position::ROOT.to_option()],
                down_context: vec![],
                start: name_start,
                end: name_end,
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                inode: Position::ROOT.to_option(),
            }),
            add_inode: Atom::NewVertex(NewVertex {
                up_context: vec![Position {
                    change: None,
                    pos: name_end,
                }],
                down_context: vec![],
                start: inode_pos,
                end: inode_pos,
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                inode: Position::ROOT.to_option(),
            }),
            contents: None,
            path: name.to_string(),
            encoding: None,
        });
    }
    let hunks: Vec<_> = hunks
        .into_iter()
        .map(|h| h.globalize(txn).unwrap())
        .collect();
    Change::make_change(
        txn,
        channel,
        hunks,
        contents,
        crate::change::ChangeHeader {
            message: "zero-root init".to_string(),
            authors: vec![],
            description: None,
            timestamp: jiff::Timestamp::now(),
        },
        Vec::new(),
    )
    .unwrap()
}

/// Regression test for the cross-root detection: in a legacy "zero-root"
/// repository (top-level files hang straight off ROOT), each top-level file's
/// NAME vertex sits directly under ROOT. Before the fix, `is_sub_root_name`
/// treated *any* such NAME vertex as a distinct sub-root, so a change touching
/// two top-level files was falsely reported as spanning two sub-roots (and the
/// CLI would refuse it as a cross-root record). A genuine sub-root NAME vertex
/// is the *empty* one minted by `AddRoot`; a non-empty top-level file name is
/// not one, so the whole repo must resolve to a single sub-root.
#[test]
fn zero_root_top_level_files_are_one_sub_root() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let changes = changestore::memory::Memory::new();
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;

    // Apply the hand-crafted zero-root change adding "foo" and "bar" directly
    // under ROOT, then materialise them in the working copy.
    let mut init = zero_root_add(&*txn.write(), &channel, &["foo", "bar"]);
    let h = changes.save_change(&mut init, |_, _| Ok::<_, anyhow::Error>(()))?;
    apply::apply_change(&changes, &mut *txn.write(), &mut *channel.write(), &h)?;
    let repo = working_copy::memory::Memory::new();
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)?;
    let mut files = repo.list_files();
    files.sort();
    assert_eq!(files, vec!["bar".to_string(), "foo".to_string()]);

    // Sanity-check the layout really is zero-root: the two names hang off ROOT.
    {
        let txn_ = txn.read();
        let ch = channel.read();
        let mut roots = 0;
        for e in iter_adjacent(
            &*txn_,
            txn_.graph(&*ch),
            Vertex::ROOT,
            EdgeFlags::FOLDER | EdgeFlags::BLOCK,
            EdgeFlags::all(),
        )? {
            let e = e?;
            if e.flag().is_folder() && e.flag().is_block() && !e.flag().is_parent() {
                let child = txn_.find_block(txn_.graph(&*ch), e.dest()).unwrap();
                // Legacy layout: the children of ROOT are the *non-empty* file
                // name vertices, not empty AddRoot markers.
                assert!(!child.is_empty(), "expected zero-root (non-empty) child");
                roots += 1;
            }
        }
        assert_eq!(roots, 2);
    }

    // Modify both top-level files and record: a single sub-root, not two.
    repo.write_file("foo", Inode::ROOT)?
        .write_all(b"foo-changed\n")?;
    repo.write_file("bar", Inode::ROOT)?
        .write_all(b"bar-changed\n")?;
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );
    Ok(())
}

/// Regression test for the false "2 independent roots" reported when recording
/// an ordinary single-project repository (e.g. Pijul itself). After the initial
/// record establishes the sub-root, adding a *new top-level file* used to
/// resolve to a brand-new sub-root (its only tree ancestor is `Inode::ROOT`, and
/// the walk bailed to `SubRoot::New` there), so a record adding a top-level file
/// alongside any edit was misreported as spanning two sub-roots. A new top-level
/// entry belongs to the existing root project: the record must stay one group.
#[test]
fn add_top_level_file_to_existing_sub_root_is_one_sub_root() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    // Initial project: a top-level file and a nested one, so the multi-root
    // layout (empty AddRoot passthrough) is established.
    for p in ["top", "sub/nested"] {
        repo.add_file(p, b"x\ny\nz\n".to_vec());
    }
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    for p in ["top", "sub/nested"] {
        txn.write().add_file(p, 0)?;
    }
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );

    // Now add a brand-new top-level file and record: still one sub-root, no
    // spurious "new project".
    repo.add_file("newtop", b"hello\n".to_vec());
    txn.write().add_file("newtop", 0)?;
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );
    Ok(())
}

/// Regression test for the false "2 independent roots" hitting repositories
/// whose root change was applied by [`apply::apply_root_change`] (the CLI path:
/// `pijul record` calls it before recording) rather than minted by the record
/// itself (`add_root_if_needed`). Only the latter registered the
/// `InodeUpdate::Add { inode: Inode::ROOT, .. }` that maps `Inode::ROOT` to the
/// root's INODE in the `inodes` table; without that entry, `inode_sub_root`
/// cannot attribute a *new top-level file* to the existing root project, so any
/// record combining one with an edit was refused as spanning two sub-roots.
/// `apply_root_change` must now write the mapping when it creates the root, and
/// repair it when called on a repository that lacks it.
#[test]
fn cli_root_change_path_maps_root_inode() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let repo = working_copy::memory::Memory::new();
    let changes = changestore::memory::Memory::new();
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    // CLI path: the root change is applied up front, not minted by the record.
    apply::apply_root_change(&mut *txn.write(), &channel, &changes, rand::rng())?.unwrap();

    for p in ["top", "sub/nested"] {
        repo.add_file(p, b"x\ny\n".to_vec());
        txn.write().add_file(p, 0)?;
    }
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );

    // New top-level file + an edit: must stay a single group.
    repo.add_file("newtop", b"hello\n".to_vec());
    txn.write().add_file("newtop", 0)?;
    repo.write_file("sub/nested", Inode::ROOT)?
        .write_all(b"changed\n")?;
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );

    // Repair path: a repository whose root was applied by an older version has
    // no `Inode::ROOT` entry. Drop it, call `apply_root_change` again (as every
    // `pijul record` does): it must restore the mapping and the next record
    // must again be a single group.
    {
        let mut txn_ = txn.write();
        let pos = *txn_.get_inodes(&Inode::ROOT, None)?.unwrap();
        crate::pristine::del_inodes_with_rev(&mut *txn_, &Inode::ROOT, &pos)?;
    }
    assert!(
        apply::apply_root_change(&mut *txn.write(), &channel, &changes, rand::rng())?.is_none()
    );
    assert!(txn.read().get_inodes(&Inode::ROOT, None)?.is_some());
    repo.add_file("newtop2", b"world\n".to_vec());
    txn.write().add_file("newtop2", 0)?;
    repo.write_file("sub/nested", Inode::ROOT)?
        .write_all(b"changed again\n")?;
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );
    Ok(())
}

/// Record the whole working copy of `repo` and return the globalized hunks
/// *without* applying them, so a test can inspect what the record produced.
fn record_hunks<
    T: MutTxnT + Send + Sync + 'static,
    R: WorkingCopy + Clone + Send + Sync + 'static,
    P: ChangeStore + Clone + Send + 'static,
>(
    repo: &R,
    store: &P,
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
) -> Result<Vec<crate::change::Hunk<Option<Hash>, crate::change::Local>>, anyhow::Error>
where
    R::Error: Send + Sync + 'static,
{
    use crate::record::{Algorithm, Builder};
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
    Ok(rec
        .actions
        .into_iter()
        .map(|r| r.globalize(&*txn.read()).unwrap())
        .collect())
}

/// Regression test: a repository merged from two *independent* repos has two
/// **alive** root vertices under ROOT (this is not a sub-module — there is no
/// relocation, so neither root's `ROOT → empty-name` edge is deleted). The
/// record traversal calls `push_children` once per alive root and therefore
/// re-walks the shared working-copy tree under `Inode::ROOT` once per root. A
/// newly-added top-level *file* used to be emitted once per root: `add_file`
/// returned `None` for files, so the file's inode was never marked recorded in
/// `recorded_inodes`, and each extra walk produced another `FileAdd` for the
/// same path — duplicate additions that then conflict on the name. The file
/// must be recorded exactly once.
#[test]
fn merged_repos_new_top_level_file_recorded_once() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let changes = changestore::memory::Memory::new();

    // Repo A.
    let repo = working_copy::memory::Memory::new();
    repo.add_file("a/x", b"a\nb\n".to_vec());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("a/x", 0)?;
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    // Repo B, recorded in its own pristine (distinct root), sharing the store.
    let repo_b = working_copy::memory::Memory::new();
    repo_b.add_file("p/q", b"m\nn\n".to_vec());
    let env_b = pristine::sanakirja::Pristine::new_anon()?;
    let txn_b = env_b.arc_txn_begin().unwrap();
    txn_b.write().add_file("p/q", 0)?;
    let channel_b = txn_b
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let b0 = record_all(&repo_b, &changes, &txn_b, &channel_b, "")?;
    txn_b.commit().unwrap();

    // Merge B into A: the channel now has two alive roots under ROOT.
    apply::apply_change(&changes, &mut *txn.write(), &mut *channel.write(), &b0)?;
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)?;

    // Add a brand-new top-level file and record.
    repo.add_file("newf", b"n\n".to_vec());
    txn.write().add_file("newf", 0)?;
    let hunks = record_hunks(&repo, &changes, &txn, &channel)?;
    let n = hunks
        .iter()
        .filter(|h| matches!(h, crate::change::Hunk::FileAdd { path, .. } if path == "newf"))
        .count();
    assert_eq!(n, 1, "expected exactly one FileAdd for `newf`, got {}", n);
    Ok(())
}

/// The empty NAME children of ROOT (each an existing sub-root's name vertex).
fn root_empty_names<T: TxnT>(
    txn: &T,
    channel: &ChannelRef<T>,
) -> Result<std::collections::HashSet<Vertex<ChangeId>>, anyhow::Error> {
    let f0 = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
    let f1 = f0 | EdgeFlags::PSEUDO;
    let t = txn;
    let ch = channel.read();
    let graph = t.graph(&*ch);
    let mut names = std::collections::HashSet::new();
    for e in iter_adjacent(t, graph, Vertex::ROOT, f0, f1)? {
        let e = e?;
        if e.flag().is_parent() {
            continue;
        }
        let child = *t.find_block(graph, e.dest()).unwrap();
        if child.start == child.end {
            names.insert(child);
        }
    }
    Ok(names)
}

/// End-to-end regression for the `clone --into` mount. Relocating an imported
/// sub-root under a directory must leave the working copy fully recorded: the
/// next `record` sees nothing. Before the fix, the record traversal parented the
/// sub-root's files on the mount *directory* — their real graph parent is the
/// mounted SUBROOT-INODE, reached through an empty, working-copy-less NAME — and
/// `collect_former_parents` reported every existing file as `is_deleted`, so the
/// record produced spurious moves that flattened the sub-root away. This checks
/// all three fixes together: [`crate::pristine::resolve_sub_root_mount`], the
/// `is_deleted` initial value, and `relocate_sub_root`'s directory metadata.
#[test]
fn relocated_sub_root_records_clean() -> Result<(), anyhow::Error> {
    env_logger::try_init().unwrap_or(());
    let changes = changestore::memory::Memory::new();

    // Repo A — the monorepo root.
    let repo = working_copy::memory::Memory::new();
    repo.add_file("a.txt", b"a\n".to_vec());
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    txn.write().add_file("a.txt", 0)?;
    let channel = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    record_all(&repo, &changes, &txn, &channel, "")?;

    // Snapshot A's sub-root NAME(s) and pick a destination parent: the INODE of
    // the existing (A) sub-root, under which `depb` will be created. Mirrors
    // `clone --into`'s `run_into`.
    let pre = root_empty_names(&*txn.read(), &channel)?;
    let dest_parent = {
        let t = txn.read();
        let ch = channel.read();
        let graph = t.graph(&*ch);
        let f0 = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
        let f1 = f0 | EdgeFlags::PSEUDO;
        let name = *pre.iter().next().unwrap();
        let e = iter_adjacent(&*t, graph, name, f0, f1)?.next().unwrap()?;
        let inode = *t.find_block(graph, e.dest()).unwrap();
        Position {
            change: inode.change,
            pos: inode.start,
        }
    };

    // Repo B, recorded in its own pristine (its own root), applied into A.
    let repo_b = working_copy::memory::Memory::new();
    repo_b.add_file("f_depB.txt", b"b\n".to_vec());
    let env_b = pristine::sanakirja::Pristine::new_anon()?;
    let txn_b = env_b.arc_txn_begin().unwrap();
    txn_b.write().add_file("f_depB.txt", 0)?;
    let channel_b = txn_b
        .write()
        .open_or_create_channel(&SmallString::from_str("main"))?;
    let b0 = record_all(&repo_b, &changes, &txn_b, &channel_b, "")?;
    txn_b.commit().unwrap();
    apply::apply_change(&changes, &mut *txn.write(), &mut *channel.write(), &b0)?;

    // The imported sub-root: the fresh empty NAME child of ROOT.
    let b_name = {
        let post = root_empty_names(&*txn.read(), &channel)?;
        *post
            .difference(&pre)
            .next()
            .expect("imported sub-root name")
    };

    // Relocate B under `depb/`, apply, and materialise the working copy.
    let header = crate::change::ChangeHeader {
        message: "Relocate cloned sub-root under depb/".to_string(),
        authors: vec![],
        description: None,
        timestamp: jiff::Timestamp::now(),
    };
    let mut reloc = crate::record::relocate_sub_root(
        &*txn.read(),
        &channel,
        dest_parent,
        b_name,
        "depb",
        header,
    )
    .map_err(|e| anyhow::anyhow!("relocate_sub_root: {:?}", e))?;
    let rh = changes.save_change(&mut reloc, |_, _| Ok::<_, anyhow::Error>(()))?;
    apply::apply_change(&changes, &mut *txn.write(), &mut *channel.write(), &rh)?;
    output::output_repository_no_pending(&repo, &changes, &txn, &channel, "", true, None, 1, 0)?;

    let files = repo.list_files();
    assert!(
        files.contains(&"a.txt".to_string()) && files.contains(&"depb/f_depB.txt".to_string()),
        "imported file not materialised under depb/: {:?}",
        files
    );

    // The acid test: recording the freshly-output working copy must produce
    // nothing — no spurious moves, so the sub-root survives.
    let hunks = record_hunks(&repo, &changes, &txn, &channel)?;
    assert!(
        hunks.is_empty(),
        "expected a clean record after relocation, got {} hunk(s): {:#?}",
        hunks.len(),
        hunks
            .iter()
            .map(|h| h.path().to_string())
            .collect::<Vec<_>>()
    );

    // Also: a new top-level file *together with* an edit to an existing file
    // (the exact shape that triggered the false cross-root error) is one group.
    repo.add_file("newtop2", b"world\n".to_vec());
    txn.write().add_file("newtop2", 0)?;
    repo.write_file("top", Inode::ROOT)?
        .write_all(b"edited\n")?;
    assert_eq!(
        record_and_count_sub_roots(&repo, &changes, &txn, &channel)?,
        1
    );
    Ok(())
}

fn bs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

/// A move crosses iff the two endpoints resolve to different boundaries.
fn crosses(old: &str, new: &str, b: &[String]) -> bool {
    boundary_of(old, b) != boundary_of(new, b)
}

#[test]
fn boundary_longest_match_and_components() {
    let b = bs(&["libs", "libs/foo"]);
    // longest match wins (nesting)
    assert_eq!(boundary_of("libs/foo/x", &b), Some("libs/foo"));
    assert_eq!(boundary_of("libs/bar/x", &b), Some("libs"));
    assert_eq!(boundary_of("libs", &b), Some("libs"));
    assert_eq!(boundary_of("libs/foo", &b), Some("libs/foo"));
    // component-aware: `libs/foo` must NOT own `libs/foobar`
    assert_eq!(boundary_of("libs/foobar", &b), Some("libs"));
    let b2 = bs(&["libs/foo"]);
    assert_eq!(boundary_of("libs/foobar", &b2), None);
    // unowned zone
    assert_eq!(boundary_of("apps/bar/x", &b), None);
}

#[test]
fn boundary_crossing_is_symmetric_both_directions() {
    let b = bs(&["libs", "libs/foo"]);
    // child -> ancestor
    assert!(crosses("libs/foo/a", "libs/b", &b));
    // ancestor -> child (the direction an asymmetric containment test misses)
    assert!(crosses("libs/a", "libs/foo/b", &b));
    // both directions between two sibling boundaries
    let b2 = bs(&["libs", "apps"]);
    assert!(crosses("libs/a", "apps/b", &b2));
    assert!(crosses("apps/b", "libs/a", &b2));
}

#[test]
fn boundary_same_and_unowned_do_not_cross() {
    let b = bs(&["libs/foo"]);
    // pure rename within a boundary
    assert!(!crosses("libs/foo/a", "libs/foo/b", &b));
    // move within the same nested boundary
    assert!(!crosses("libs/foo/a/x", "libs/foo/c/y", &b));
    // two unowned paths share the residual zone -> not a crossing
    assert!(!crosses("apps/a", "tools/b", &b));
}

#[test]
fn boundary_leaving_into_unowned_crosses() {
    let b = bs(&["libs/foo"]);
    assert!(crosses("libs/foo/a", "scratch/a", &b));
    assert!(crosses("scratch/a", "libs/foo/a", &b));
}

#[test]
fn boundary_no_boundaries_never_crosses() {
    let b: Vec<String> = Vec::new();
    assert_eq!(boundary_of("libs/foo/x", &b), None);
    assert!(!crosses("libs/foo/a", "apps/bar/b", &b));
}
