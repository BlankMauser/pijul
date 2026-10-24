//! Squash and conflicts. A change is never in conflict with itself, so a squash
//! is refused whenever the squashed changes leave a conflict in the files they
//! touch, and accepted when the resolution is part of the squash. Every kind of
//! conflict is covered, left unresolved, resolved inside the squash, resolved
//! before, between two squashed changes, or in another file.

use super::squash::{on_channel, pending, squash_with};
use super::*;
use crate::TxnTExt;
use crate::change::{ChangeHeader, Hunk};
use crate::output::Conflict;
use crate::squash::SquashError;
use crate::working_copy::{WorkingCopy, WorkingCopyRead};
use std::io::Write;

type Store = changestore::memory::Memory;
type Txn = pristine::sanakirja::MutTxn0;

/// A repository of its own (pristine, channel, working copy), sharing the
/// change store with the other peers of a test.
struct Peer {
    _env: pristine::sanakirja::Pristine,
    txn: ArcTxn<Txn>,
    channel: ChannelRef<Txn>,
    repo: working_copy::memory::Memory,
}

impl Peer {
    fn new() -> Self {
        let env = pristine::sanakirja::Pristine::new_anon().unwrap();
        let txn = env.arc_txn_begin().unwrap();
        let channel = txn
            .write()
            .open_or_create_channel(&SmallString::from_str("main"))
            .unwrap();
        Peer {
            _env: env,
            txn,
            channel,
            repo: working_copy::memory::Memory::new(),
        }
    }

    /// A peer whose first change adds `files`.
    fn init(changes: &Store, files: &[(&str, &[u8])]) -> (Self, Hash) {
        let p = Peer::new();
        for (f, c) in files {
            p.repo.add_file(f, c.to_vec());
            p.txn.write().add_file(f, 0).unwrap();
        }
        let h = p.record(changes);
        (p, h)
    }

    /// A peer that applied `hashes`, in this order.
    fn clone_of(changes: &Store, hashes: &[Hash]) -> Self {
        let p = Peer::new();
        for h in hashes {
            apply::apply_change_arc(changes, &p.txn, &p.channel, h).unwrap();
        }
        p.output(changes);
        p
    }

    /// Apply `h` and output the working copy.
    fn pull(&self, changes: &Store, h: &Hash) -> Vec<Conflict> {
        apply::apply_change_arc(changes, &self.txn, &self.channel, h).unwrap();
        self.output(changes)
    }

    fn output(&self, changes: &Store) -> Vec<Conflict> {
        output::output_repository_no_pending(
            &self.repo,
            changes,
            &self.txn,
            &self.channel,
            "",
            true,
            None,
            1,
            0,
        )
        .unwrap()
        .into_iter()
        .collect()
    }

    fn write(&self, path: &str, contents: &[u8]) {
        self.repo
            .write_file(path, Inode::ROOT)
            .unwrap()
            .write_all(contents)
            .unwrap();
    }

    fn read(&self, path: &str) -> Vec<u8> {
        let mut b = Vec::new();
        self.repo.read_file(path, &mut b).unwrap();
        b
    }

    fn record(&self, changes: &Store) -> Hash {
        record_all(&self.repo, changes, &self.txn, &self.channel, "").unwrap()
    }

    fn mv(&self, from: &str, to: &str) {
        self.repo.rename(from, to).unwrap();
        self.txn.write().move_file(from, to, 0).unwrap();
    }

    /// Resolve the conflicts in `path` by keeping all the lines shown.
    fn keep_all(&self, changes: &Store, path: &str) -> Hash {
        let c = self.read(path);
        self.write(path, &strip_markers(&c));
        self.record(changes)
    }

    /// The channel's changes, in application order.
    fn log(&self) -> Vec<Hash> {
        let txn = self.txn.read();
        let mut l: Vec<Hash> = txn
            .reverse_log(&*self.channel.read(), None)
            .unwrap()
            .map(|h| (h.unwrap().1).0.into())
            .collect();
        l.reverse();
        l
    }

    fn files(&self) -> Vec<String> {
        let mut f: Vec<_> = self
            .repo
            .list_files()
            .into_iter()
            .filter(|f| !self.repo.file_metadata(f).unwrap().is_dir())
            .collect();
        f.sort();
        f
    }
}

fn strip_markers(c: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in c.split_inclusive(|&b| b == b'\n') {
        if !(l.starts_with(b">>>>>>>") || l.starts_with(b"=======") || l.starts_with(b"<<<<<<<")) {
            out.extend_from_slice(l)
        }
    }
    out
}

type Snapshot = (Vec<(String, Vec<u8>)>, usize);

/// Every file of the channel and the number of conflicts, output in a scratch
/// tree so that the peer's own tree is left alone.
fn snapshot(p: &Peer, changes: &Store) -> Snapshot {
    p.txn.write().begin_scratch_tree().unwrap();
    let view = working_copy::memory::Memory::new();
    let conflicts = output::output_repository_no_pending_(
        &view, changes, &p.txn, &p.channel, "", true, None, 1, 0,
    )
    .unwrap();
    p.txn.write().end_scratch_tree().unwrap();
    // Which of its names a file with several names is output under is not
    // deterministic: hide that name.
    let multiple: Vec<String> = conflicts
        .iter()
        .filter_map(|c| match c {
            Conflict::MultipleNames { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let mut files = Vec::new();
    for f in view.list_files() {
        let contents = if view.file_metadata(&f).unwrap().is_dir() {
            Vec::new()
        } else {
            let mut b = Vec::new();
            view.read_file(&f, &mut b).unwrap();
            b
        };
        if multiple.contains(&f) {
            files.push(("<one of several names>".to_string(), contents))
        } else {
            files.push((f, contents))
        }
    }
    files.sort();
    (files, conflicts.len())
}

/// Squash `hashes` on `p` and check that nothing visible changed: same files,
/// same conflicts, same unrecorded changes; and that a fresh peer replaying the
/// new log gets the same files.
fn accept(p: &Peer, changes: &Store, hashes: &[Hash], force: bool) -> Hash {
    let before = snapshot(p, changes);
    let pending_before = pending(&p.repo, changes, &p.txn, &p.channel);
    let s = squash_with(&p.txn, &p.channel, changes, hashes, force).unwrap();
    if std::env::var("SQUASH_DEBUG").is_ok() {
        for h in hashes.iter().chain(std::iter::once(&s)) {
            eprintln!("===== {}", h.to_base32());
            changes
                .get_change(h)
                .unwrap()
                .write(changes, Some(*h), true, &mut std::io::stderr())
                .unwrap();
        }
    }
    for h in hashes {
        assert!(!on_channel(&p.txn, &p.channel, h));
    }
    assert!(on_channel(&p.txn, &p.channel, &s));
    assert_eq!(snapshot(p, changes), before);
    assert_eq!(
        pending(&p.repo, changes, &p.txn, &p.channel),
        pending_before
    );
    let replay = Peer::clone_of(changes, &p.log());
    assert_eq!(snapshot(&replay, changes), before);
    s
}

/// The squash of `hashes` on `p` is refused for conflicts, and `p` is left
/// exactly as it was.
fn refuse(p: &Peer, changes: &Store, hashes: &[Hash]) -> Vec<Conflict> {
    let before = snapshot(p, changes);
    let log = p.log();
    let conflicts = match crate::squash::squash(
        &p.txn,
        &p.channel,
        changes,
        hashes,
        ChangeHeader::default(),
        true,
    ) {
        Err(SquashError::Conflicts { conflicts }) => conflicts,
        Err(e) => panic!("unexpected error {:?}", e),
        Ok(_) => panic!("the squash should have been refused"),
    };
    assert!(!conflicts.is_empty());
    assert_eq!(p.log(), log);
    assert_eq!(snapshot(p, changes), before);
    conflicts
}

// ---------------------------------------------------------------------------
// Order conflicts: concurrent insertions at the same place.

struct Order {
    changes: Store,
    alice: Peer,
    bob: Peer,
    h1: Hash,
    h2: Hash,
    hb: Hash,
}

/// Alice inserts x then X after a, Bob inserts y there, Alice pulls Bob.
fn order() -> Order {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("file", b"a\nb\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.write("file", b"a\nx\nb\n");
    let h1 = alice.record(&changes);
    alice.write("file", b"a\nx\nX\nb\n");
    let h2 = alice.record(&changes);
    bob.write("file", b"a\ny\nb\n");
    let hb = bob.record(&changes);
    let c = alice.pull(&changes, &hb);
    assert!(
        c.iter().any(|c| matches!(c, Conflict::Order { .. })),
        "{:?}",
        c
    );
    Order {
        changes,
        alice,
        bob,
        h1,
        h2,
        hb,
    }
}

#[test]
fn order_unresolved_is_refused() {
    let o = order();
    refuse(&o.alice, &o.changes, &[o.h1, o.h2]);
    refuse(&o.alice, &o.changes, &[o.h2]);
    // The other side of the conflict, on Bob's channel.
    o.bob.pull(&o.changes, &o.h1);
    o.bob.pull(&o.changes, &o.h2);
    refuse(&o.bob, &o.changes, &[o.hb]);
}

#[test]
fn order_resolution_inside_squash() {
    let o = order();
    let hr = o.alice.keep_all(&o.changes, "file");
    accept(&o.alice, &o.changes, &[o.h1, o.h2, hr], false);
}

#[test]
fn order_resolution_with_last_side() {
    let o = order();
    let hr = o.alice.keep_all(&o.changes, "file");
    accept(&o.alice, &o.changes, &[o.h2, hr], false);
}

#[test]
fn order_resolution_only() {
    // The conflict is resolved by a squashed change; its sides are not squashed.
    let o = order();
    let hr = o.alice.keep_all(&o.changes, "file");
    let mut c = o.alice.read("file");
    c.extend_from_slice(b"end\n");
    o.alice.write("file", &c);
    let he = o.alice.record(&o.changes);
    accept(&o.alice, &o.changes, &[hr, he], false);
}

#[test]
fn conflict_between_squashed_changes() {
    // Both sides of the conflict are squashed: without the resolution, the new
    // change would conflict with itself.
    let o = order();
    refuse(&o.alice, &o.changes, &[o.h1, o.h2, o.hb]);
    let hr = o.alice.keep_all(&o.changes, "file");
    let s = accept(&o.alice, &o.changes, &[o.h1, o.h2, o.hb, hr], false);
    assert_eq!(snapshot(&o.alice, &o.changes).1, 0);
    assert!(!o.changes.get_change(&s).unwrap().changes.is_empty());
}

#[test]
fn order_resolved_squash_applies_on_peer() {
    // Bob has the conflict and Alice's resolution; Alice squashes it all.
    let o = order();
    let hr = o.alice.keep_all(&o.changes, "file");
    for h in [o.h1, o.h2, hr] {
        o.bob.pull(&o.changes, &h);
    }
    let bob_before = snapshot(&o.bob, &o.changes);
    let s = accept(&o.alice, &o.changes, &[o.h1, o.h2, hr], false);
    use crate::MutTxnTExt;
    o.bob
        .txn
        .write()
        .unrecord_superseded(&o.changes, &o.bob.channel, &s)
        .unwrap();
    o.bob.pull(&o.changes, &s);
    assert!(!on_channel(&o.bob.txn, &o.bob.channel, &hr));
    assert_eq!(snapshot(&o.bob, &o.changes), bob_before);
    assert_eq!(snapshot(&o.bob, &o.changes), snapshot(&o.alice, &o.changes));
}

#[test]
fn conflict_in_another_file_is_kept() {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("f", b"1\n2\n"), ("g", b"a\nb\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.write("g", b"a\nx\nb\n");
    alice.record(&changes);
    bob.write("g", b"a\ny\nb\n");
    let hb = bob.record(&changes);
    assert!(!alice.pull(&changes, &hb).is_empty());

    alice.write("f", b"1\nX\n2\n");
    let e1 = alice.record(&changes);
    alice.write("f", b"1\nX\nY\n2\n");
    let e2 = alice.record(&changes);
    accept(&alice, &changes, &[e1, e2], false);
    assert!(snapshot(&alice, &changes).1 > 0);
}

#[test]
fn conflict_elsewhere_in_the_same_file_is_refused() {
    // The squashed change does not take part in the conflict, but touches the
    // conflicted file: the squash would have to record over the conflict.
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("f", b"a\nb\nc\nd\ne\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.write("f", b"a\nx\nb\nc\nd\ne\n");
    alice.record(&changes);
    bob.write("f", b"a\ny\nb\nc\nd\ne\n");
    let hb = bob.record(&changes);
    assert!(!alice.pull(&changes, &hb).is_empty());
    let mut c = alice.read("f");
    c.extend_from_slice(b"z\n");
    alice.write("f", &c);
    let e = alice.record(&changes);
    refuse(&alice, &changes, &[e]);
}

// ---------------------------------------------------------------------------
// Zombies: lines deleted on one side, used as context on the other.

struct Zombie {
    changes: Store,
    alice: Peer,
    h1: Hash,
    h2: Hash,
}

fn zombie() -> Zombie {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("file", b"a\nb\nc\nd\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.write("file", b"a\nd\n");
    let h1 = alice.record(&changes);
    alice.write("file", b"a\nd\ne\n");
    let h2 = alice.record(&changes);
    bob.write("file", b"a\nb\nx\nc\nd\n");
    let hb = bob.record(&changes);
    let c = alice.pull(&changes, &hb);
    assert!(
        c.iter().any(|c| matches!(c, Conflict::Zombie { .. })),
        "{:?}",
        c
    );
    Zombie {
        changes,
        alice,
        h1,
        h2,
    }
}

#[test]
fn zombie_unresolved_is_refused() {
    let z = zombie();
    refuse(&z.alice, &z.changes, &[z.h1, z.h2]);
    refuse(&z.alice, &z.changes, &[z.h1]);
}

#[test]
fn zombie_kept_inside_squash() {
    let z = zombie();
    let hr = z.alice.keep_all(&z.changes, "file");
    accept(&z.alice, &z.changes, &[z.h1, z.h2, hr], false);
}

#[test]
fn zombie_deleted_inside_squash() {
    let z = zombie();
    z.alice.write("file", b"a\nd\ne\n");
    let hr = z.alice.record(&z.changes);
    accept(&z.alice, &z.changes, &[z.h1, hr], false);
}

#[test]
fn zombie_resolution_only() {
    let z = zombie();
    let hr = z.alice.keep_all(&z.changes, "file");
    accept(&z.alice, &z.changes, &[hr], false);
}

// ---------------------------------------------------------------------------
// Cyclic conflicts: a resolution of Alice and Bob's conflict, confronted with
// a third, independent side (Charlie).

struct Cyclic {
    changes: Store,
    bob: Peer,
    bobs_resolution: Hash,
}

fn cyclic() -> Cyclic {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("file", b"a\nb\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    let charlie = Peer::clone_of(&changes, &[h0]);
    charlie.write("file", b"a\nU\nV\nW\nb\n");
    let charlie_h = charlie.record(&changes);
    bob.write("file", b"a\nu\nv\nw\nb\n");
    let bob_h = bob.record(&changes);
    alice.write("file", b"a\nx\ny\nz\nb\n");
    let alice_h = alice.record(&changes);

    alice.pull(&changes, &bob_h);
    let alices_resolution = alice.keep_all(&changes, "file");
    bob.pull(&changes, &alice_h);
    let bobs_resolution = bob.keep_all(&changes, "file");
    bob.pull(&changes, &alices_resolution);
    let c = bob.pull(&changes, &charlie_h);
    assert!(!c.is_empty());
    assert!(
        c.iter()
            .any(|c| matches!(c, Conflict::Cyclic { .. } | Conflict::Order { .. })),
        "{:?}",
        c
    );
    Cyclic {
        changes,
        bob,
        bobs_resolution,
    }
}

#[test]
fn cyclic_unresolved_is_refused() {
    let c = cyclic();
    refuse(&c.bob, &c.changes, &[c.bobs_resolution]);
}

#[test]
fn cyclic_resolution_inside_squash() {
    let c = cyclic();
    let second = c.bob.keep_all(&c.changes, "file");
    accept(&c.bob, &c.changes, &[c.bobs_resolution, second], false);
}

#[test]
fn cyclic_resolution_only() {
    let c = cyclic();
    let second = c.bob.keep_all(&c.changes, "file");
    accept(&c.bob, &c.changes, &[second], false);
}

// ---------------------------------------------------------------------------
// Name conflicts: two files moved to the same name.

#[test]
fn name_conflict() {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("file1", b"one\n"), ("file2", b"two\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.mv("file1", "file");
    let h1 = alice.record(&changes);
    bob.mv("file2", "file");
    let hb = bob.record(&changes);
    let c = alice.pull(&changes, &hb);
    assert!(
        c.iter().any(|c| matches!(c, Conflict::Name { .. })),
        "{:?}",
        c
    );
    refuse(&alice, &changes, &[h1]);

    let files = alice.files();
    assert_eq!(files.len(), 2, "{:?}", files);
    let other = files.iter().find(|f| f.as_str() != "file").unwrap().clone();
    alice.mv(&other, "renamed");
    let hr = alice.record(&changes);
    accept(&alice, &changes, &[h1, hr], false);
    assert_eq!(snapshot(&alice, &changes).1, 0);
}

// ---------------------------------------------------------------------------
// Multiple names: one file moved to two names.

#[test]
fn multiple_names_conflict() {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("f", b"x\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.mv("f", "g");
    let h1 = alice.record(&changes);
    bob.mv("f", "h");
    let hb = bob.record(&changes);
    let c = alice.pull(&changes, &hb);
    assert!(
        c.iter()
            .any(|c| matches!(c, Conflict::MultipleNames { .. })),
        "{:?}",
        c
    );
    refuse(&alice, &changes, &[h1]);

    // Recording the working copy picks the name that was output.
    let hr = alice.record(&changes);
    assert_eq!(snapshot(&alice, &changes).1, 0);
    accept(&alice, &changes, &[h1, hr], false);
}

/// The resolution keeps the name given by the squashed change: the youngest
/// name of the file (from the resolution) is then a deleted one.
#[test]
fn multiple_names_resolved_to_squashed_name() {
    multiple_names_resolved_to("g")
}

#[test]
fn multiple_names_resolved_to_other_name() {
    multiple_names_resolved_to("h")
}

fn multiple_names_resolved_to(name: &str) {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("f", b"x\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.mv("f", "g");
    let h1 = alice.record(&changes);
    bob.mv("f", "h");
    let hb = bob.record(&changes);
    assert!(!alice.pull(&changes, &hb).is_empty());

    let files = alice.files();
    assert_eq!(files.len(), 1, "{:?}", files);
    if files[0] != name {
        alice.mv(&files[0], name);
    }
    let hr = alice.record(&changes);
    let (files, conflicts) = snapshot(&alice, &changes);
    assert_eq!(conflicts, 0);
    assert_eq!(files, vec![(name.to_string(), b"x\n".to_vec())]);
    accept(&alice, &changes, &[h1, hr], false);
}

// ---------------------------------------------------------------------------
// Zombie files: a file deleted on one side, edited on the other.

#[test]
fn zombie_file_conflict() {
    // Alice moves the file, Bob deletes it and pulls the move (as in
    // `file_conflicts::zombie_file_post_resolve`).
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("a/b/c/file", b"a\nb\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.mv("a/b/c/file", "a/b/c/alice");
    let ha = alice.record(&changes);
    bob.repo.remove_path("a/b/c/file", false).unwrap();
    let h1 = bob.record(&changes);
    bob.write("top", b"t\n");
    bob.txn.write().add_file("top", 0).unwrap();
    let h2 = bob.record(&changes);
    let c = bob.pull(&changes, &ha);
    assert!(
        c.iter().any(|c| matches!(c, Conflict::ZombieFile { .. })),
        "{:?}",
        c
    );
    refuse(&bob, &changes, &[h1]);
    refuse(&bob, &changes, &[h1, h2]);

    // Keep the file.
    let hr = bob.record(&changes);
    assert_eq!(snapshot(&bob, &changes).1, 0);
    accept(&bob, &changes, &[h1, hr], false);
}

#[test]
fn zombie_file_deleted_again() {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("a/b/c/file", b"a\nb\n"), ("top", b"t\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.repo.remove_path("a/b", true).unwrap();
    let h1 = alice.record(&changes);
    bob.write("a/b/c/file", b"a\nx\nb\n");
    let hb = bob.record(&changes);
    assert!(!alice.pull(&changes, &hb).is_empty());

    alice.repo.remove_path("a/b", true).unwrap();
    let hr = alice.record(&changes);
    assert_eq!(snapshot(&alice, &changes).1, 0);
    accept(&alice, &changes, &[h1, hr], false);
}

// ---------------------------------------------------------------------------
// Not conflicts.

#[test]
fn literal_conflict_markers() {
    // Marker-like lines recorded as content are not conflicts.
    let changes = Store::new();
    let (alice, _) = Peer::init(&changes, &[("file", b"a\n")]);
    alice.write("file", b"a\n>>>>>>> 1\nx\n=======\ny\n<<<<<<< 1\n");
    let h1 = alice.record(&changes);
    alice.write("file", b"a\n>>>>>>> 1\nx\n=======\ny2\n<<<<<<< 1\nb\n");
    let h2 = alice.record(&changes);
    accept(&alice, &changes, &[h1, h2], false);
    let (files, conflicts) = snapshot(&alice, &changes);
    assert_eq!(conflicts, 0);
    assert!(files.contains(&(
        "file".to_string(),
        b"a\n>>>>>>> 1\nx\n=======\ny2\n<<<<<<< 1\nb\n".to_vec()
    )));
}

#[test]
fn marker_line_matching_the_conflict_it_resolves() {
    // A file holds, as content, a line identical to the first marker of a
    // conflict it later has; the squashed change resolves that conflict. The
    // squash re-records the file against its conflicted version, whose
    // rendering has the same line as a marker: the result must be exact, or
    // the squash refused.
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("file", b"a\nb\n")]);
    let carol = Peer::clone_of(&changes, &[h0]);
    carol.write("file", b"a\nx\nb\n");
    let hc = carol.record(&changes);
    let bob = Peer::clone_of(&changes, &[h0]);
    bob.write("file", b"a\ny\nb\n");
    let hb = bob.record(&changes);

    let probe = Peer::clone_of(&changes, &[h0, hc, hb]);
    let marker: Vec<u8> = probe
        .read("file")
        .split_inclusive(|&b| b == b'\n')
        .find(|l| l.starts_with(b">>>>>>>"))
        .unwrap()
        .to_vec();

    let mut with_line = marker.clone();
    with_line.extend_from_slice(b"a\nb\n");
    alice.write("file", &with_line);
    alice.record(&changes);
    alice.pull(&changes, &hc);
    assert!(!alice.pull(&changes, &hb).is_empty());
    let mut resolved = marker.clone();
    resolved.extend_from_slice(&strip_markers(&alice.read("file")));
    alice.write("file", &resolved);
    let hr = alice.record(&changes);
    let (files, conflicts) = snapshot(&alice, &changes);
    assert_eq!(conflicts, 0);
    assert!(
        files.contains(&("file".to_string(), resolved.clone())),
        "{:?}",
        files
    );

    let mut s = crate::squash::squash(
        &alice.txn,
        &alice.channel,
        &changes,
        &[hr],
        ChangeHeader::default(),
        false,
    )
    .unwrap();
    let hash = changes
        .save_change(&mut s.change, |_, _| Ok::<_, anyhow::Error>(()))
        .unwrap();
    match crate::squash::apply_squash(&alice.txn, &alice.channel, &changes, &s, &hash) {
        Err(SquashError::NotEquivalent { paths }) => assert_eq!(paths, vec!["file".to_string()]),
        Err(e) => panic!("unexpected error {:?}", e),
        Ok(()) => assert!(
            snapshot(&alice, &changes)
                .0
                .contains(&("file".to_string(), resolved))
        ),
    }
}

#[test]
fn squashed_changes_cancel_out() {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("file", b"a\nb\n")]);
    alice.write("file", b"a\nx\nb\n");
    let h1 = alice.record(&changes);
    alice.write("file", b"a\nb\n");
    let h2 = alice.record(&changes);
    let bob = Peer::clone_of(&changes, &[h0, h1, h2]);

    let s = accept(&alice, &changes, &[h1, h2], false);
    assert!(changes.get_change(&s).unwrap().changes.is_empty());

    use crate::MutTxnTExt;
    bob.txn
        .write()
        .unrecord_superseded(&changes, &bob.channel, &s)
        .unwrap();
    bob.pull(&changes, &s);
    assert!(!on_channel(&bob.txn, &bob.channel, &h1));
    assert_eq!(snapshot(&bob, &changes), snapshot(&alice, &changes));
}

#[test]
fn peer_with_concurrent_conflict() {
    // Bob has the squashed changes *and* a change conflicting with them. After
    // replacing them with the squash, Bob sees the same lines, still in conflict.
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("file", b"a\nb\n")]);
    let bob = Peer::clone_of(&changes, &[h0]);
    alice.write("file", b"a\nx\nb\n");
    let h1 = alice.record(&changes);
    alice.write("file", b"a\nx\nX\nb\n");
    let h2 = alice.record(&changes);
    bob.write("file", b"a\ny\nb\n");
    bob.record(&changes);
    bob.pull(&changes, &h1);
    bob.pull(&changes, &h2);
    let before = snapshot(&bob, &changes);
    assert!(before.1 > 0);

    let s = accept(&alice, &changes, &[h1, h2], false);
    use crate::MutTxnTExt;
    bob.txn
        .write()
        .unrecord_superseded(&changes, &bob.channel, &s)
        .unwrap();
    bob.pull(&changes, &s);
    let after = snapshot(&bob, &changes);
    assert_eq!(after.1, before.1);
    let lines = |s: &Snapshot| {
        let mut l: Vec<Vec<u8>> =
            s.0.iter()
                .flat_map(|(_, c)| {
                    strip_markers(c)
                        .split(|&b| b == b'\n')
                        .map(|l| l.to_vec())
                        .collect::<Vec<_>>()
                })
                .collect();
        l.sort();
        l
    };
    assert_eq!(lines(&after), lines(&before));
}

// ---------------------------------------------------------------------------
// Roots.

#[test]
fn add_root_is_never_squashed() {
    let changes = Store::new();
    let (alice, h0) = Peer::init(&changes, &[("file", b"a\n")]);
    assert!(
        changes
            .get_change(&h0)
            .unwrap()
            .changes
            .iter()
            .any(|h| matches!(h, Hunk::AddRoot { .. }))
    );
    alice.write("file", b"a\nb\n");
    let h1 = alice.record(&changes);
    let log = alice.log();
    let before = snapshot(&alice, &changes);
    match crate::squash::squash(
        &alice.txn,
        &alice.channel,
        &changes,
        &[h0, h1],
        ChangeHeader::default(),
        true,
    ) {
        Err(SquashError::AddRoot { hash }) => assert_eq!(hash, Some(h0)),
        Err(e) => panic!("unexpected error {:?}", e),
        Ok(_) => panic!("the squash should have been refused"),
    }
    assert_eq!(alice.log(), log);
    assert_eq!(snapshot(&alice, &changes), before);
}

#[test]
fn top_level_addition_is_one_root() {
    let changes = Store::new();
    let (alice, _) = Peer::init(&changes, &[("dir/file", b"a\n")]);
    alice.repo.add_file("new", b"n\n".to_vec());
    alice.txn.write().add_file("new", 0).unwrap();
    let h1 = alice.record(&changes);
    alice.write("new", b"n\nm\n");
    let h2 = alice.record(&changes);
    accept(&alice, &changes, &[h1, h2], false);
}

/// A repository with two sub-roots (B imported under `vendor/`, as `clone
/// --into` does), and two changes, one in each.
fn two_sub_roots() -> (Store, Peer, Hash, Hash) {
    let changes = Store::new();
    let (a, _) = Peer::init(&changes, &[("a/x", b"a\nb\n")]);
    let (b, b0) = Peer::init(&changes, &[("p/q", b"m\nn\n")]);
    drop(b);

    let f0 = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
    let f1 = f0 | EdgeFlags::PSEUDO;
    let (pre_names, dest_parent) = {
        let t = a.txn.read();
        let ch = a.channel.read();
        let graph = t.graph(&*ch);
        let mut names = std::collections::HashSet::new();
        let mut dest_parent = None;
        for e in iter_adjacent(&*t, graph, Vertex::ROOT, f0, f1).unwrap() {
            let child = *t.find_block(graph, e.unwrap().dest()).unwrap();
            if child.start != child.end {
                continue;
            }
            names.insert(child);
            if dest_parent.is_none() {
                if let Some(e2) = iter_adjacent(&*t, graph, child, f0, f1).unwrap().next() {
                    let inode = *t.find_block(graph, e2.unwrap().dest()).unwrap();
                    dest_parent = Some(Position {
                        change: inode.change,
                        pos: inode.start,
                    });
                }
            }
        }
        (names, dest_parent.unwrap())
    };
    a.pull(&changes, &b0);
    let b_name = {
        let t = a.txn.read();
        let ch = a.channel.read();
        let graph = t.graph(&*ch);
        let mut found = None;
        for e in iter_adjacent(&*t, graph, Vertex::ROOT, f0, f1).unwrap() {
            let child = *t.find_block(graph, e.unwrap().dest()).unwrap();
            if child.start == child.end && !pre_names.contains(&child) {
                found = Some(child);
            }
        }
        found.unwrap()
    };
    let mut reloc = crate::record::relocate_sub_root(
        &*a.txn.read(),
        &a.channel,
        dest_parent,
        b_name,
        "vendor",
        ChangeHeader::default(),
    )
    .unwrap();
    let rh = changes
        .save_change(&mut reloc, |_, _| Ok::<_, anyhow::Error>(()))
        .unwrap();
    a.pull(&changes, &rh);

    a.write("a/x", b"a2\n");
    let h1 = a.record(&changes);
    a.write("vendor/p/q", b"m2\n");
    let h2 = a.record(&changes);
    (changes, a, h1, h2)
}

#[test]
fn several_sub_roots_need_force() {
    let (changes, a, h1, h2) = two_sub_roots();
    match crate::squash::squash(
        &a.txn,
        &a.channel,
        &changes,
        &[h1, h2],
        ChangeHeader::default(),
        false,
    ) {
        Err(SquashError::SubRoots { sub_roots }) => assert_eq!(sub_roots.len(), 2),
        Err(e) => panic!("unexpected error {:?}", e),
        Ok(_) => panic!("the squash should have been refused"),
    }

    let (changes, a, h1, h2) = two_sub_roots();
    accept(&a, &changes, &[h1, h2], true);
}
