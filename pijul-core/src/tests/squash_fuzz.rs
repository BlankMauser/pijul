//! Seeded random walk over record / pull / squash across a few channels of
//! one file. Concurrent records produce conflicts, zombies and cancelling
//! changes on their own. After every squash attempt:
//! - it is refused for conflicts if and only if the file has conflicts, and a
//!   refusal changes nothing;
//! - an accepted squash leaves the file byte for byte identical;
//! - replaying the channel's changes (with the squash) on a fresh channel gives
//!   the same file;
//! - a channel holding exactly the same changes that pulls the squash (through
//!   `unrecord_superseded`, as `pull` does) gets the same file.
//!
//! Pulls also go through `unrecord_superseded`, so squashes travel between
//! channels during the walk.
//!
//! A short run is part of the test suite. Longer:
//!   FUZZ_SQUASH_SEEDS=2000 FUZZ_SQUASH_STEPS=300 \
//!     cargo test -p pijul-core --release fuzz_squash_long -- --ignored --nocapture
//! One seed, verbosely:
//!   FUZZ_SQUASH_START=<seed> FUZZ_SQUASH_SEEDS=1 FUZZ_VERBOSE=1 \
//!     cargo test -p pijul-core --release fuzz_squash_long -- --ignored --nocapture
//!
//! Probes: `FUZZ_CHECK_GRAPH` compares each channel to a fresh replay of its
//! changes, edge by edge; `FUZZ_LOG_STEP=<step>` turns on the debug log around
//! the record of that step, and `FUZZ_DOT=<dir>` then writes the channel right
//! after it (`step<n>-c<i>-rec.dot`) as well as at the end of every step.

use super::unrecord::{fuzz_mutate, fuzz_valid_order};
use super::*;
use crate::MutTxnTExt;
use crate::change::ChangeHeader;
use crate::squash::SquashError;
use crate::working_copy::{WorkingCopy, WorkingCopyRead};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha20Rng;
use std::io::Write;

type Store = changestore::memory::Memory;

thread_local! {
    /// Check, at every step, that outputting a channel (which deletes the
    /// redundant pseudo-edges it finds) does not change what it outputs next
    /// time. Also set by `FUZZ_CHECK_REDUNDANT`, for a long run.
    static CHECK_REDUNDANT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Delete the redundant pseudo-edges of the file at `pos`, which output used to
/// do on its own: the `FUZZ_CHECK_REDUNDANT` probe needs them gone to check that
/// they do not change what the channel outputs.
fn prune_forward_edges<T: MutTxnT>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    pos: Position<ChangeId>,
) -> Result<(), anyhow::Error> {
    let mut txn = txn.write();
    let mut channel = channel.write();
    crate::alive::remove_forward_edges(&mut *txn, T::graph_mut(&mut *channel), pos)?;
    Ok(())
}

/// The sorted lines of the graph's dot dump, to compare two channels edge by
/// edge (`FUZZ_CHECK_GRAPH`).
fn graph_lines<T>(txn: &T, channel: &ChannelRef<T>) -> Vec<String>
where
    T: crate::pristine::GraphIter + ChannelTxnT,
{
    let mut buf = Vec::new();
    {
        let ch = channel.r.read();
        crate::pristine::debug(txn, txn.graph(&*ch), &mut buf).unwrap();
    }
    let mut lines: Vec<String> = String::from_utf8_lossy(&buf)
        .lines()
        .map(|s| s.to_string())
        .collect();
    lines.sort();
    lines
}

/// The file of `channel` and its number of conflicts, output in a scratch tree.
fn render<T>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    changes: &Store,
) -> Result<(Vec<u8>, usize), anyhow::Error>
where
    T: MutTxnT + Send + Sync + 'static,
    T::Channel: Send + Sync,
{
    txn.write().begin_scratch_tree()?;
    let view = working_copy::memory::Memory::new();
    let conflicts =
        output::output_repository_no_pending_(&view, changes, txn, channel, "", true, None, 1, 0)?;
    txn.write().end_scratch_tree()?;
    let mut buf = Vec::new();
    let _ = view.read_file("file", &mut buf);
    Ok((buf, conflicts.len()))
}

/// A user unrecords `squashed` (in application order) from `channel`, writes
/// `content` to the file and records it: the record a squash does, against the
/// very same graph. Returns whether they get exactly `content` without
/// conflicts, and whether replaying `survivors` and their change on a fresh
/// channel gives the same file as `channel`.
fn record_by_hand<T>(
    txn: &ArcTxn<T>,
    changes: &Store,
    channel: &ChannelRef<T>,
    squashed: &[Hash],
    survivors: &[Hash],
    content: &[u8],
) -> Result<(bool, bool), anyhow::Error>
where
    T: MutTxnT + crate::pristine::GraphIter + Send + Sync + 'static,
    T::Channel: Send + Sync,
{
    if std::env::var("FUZZ_PRINT_CHANGES").is_ok() {
        for (hs, what) in [(survivors, "survivor"), (squashed, "squashed")] {
            for h in hs.iter() {
                eprintln!("===== {} ({what})", h.to_base32());
                changes
                    .get_change(h)?
                    .write(changes, Some(*h), true, &mut std::io::stderr())?;
            }
        }
    }
    if let Ok(dir) = std::env::var("FUZZ_DOT") {
        crate::pristine::debug_to_file(&*txn.read(), channel, format!("{dir}/unrecord.dot"))?;
        // A fresh replay of the same changes, to compare with the graph the walk
        // built (`unrecord.dot`).
        let all_only = txn
            .write()
            .open_or_create_channel(&SmallString::from_str("by-hand-all"))?;
        let mut all = survivors.to_vec();
        all.extend(squashed.iter().cloned());
        for x in fuzz_valid_order(changes, &all, false) {
            apply::apply_change_arc(changes, txn, &all_only, &x)?;
        }
        crate::pristine::debug_to_file(&*txn.read(), &all_only, format!("{dir}/all.dot"))?;
    }
    // Logs of these unrecords only (the walk up to here would drown them).
    let log_unrecord = std::env::var("FUZZ_LOG_UNRECORD").is_ok();
    if log_unrecord {
        log::set_max_level(log::LevelFilter::Debug);
    }
    for h in squashed.iter().rev() {
        crate::unrecord::unrecord(
            &mut *txn.write(),
            channel,
            changes,
            h,
            0,
            &mut Default::default(),
        )?;
    }
    if log_unrecord {
        log::set_max_level(log::LevelFilter::Off);
    }
    // No edge may still reference an unrecorded change.
    for h in squashed.iter() {
        super::unrecord::check_unrec(&*txn.read(), &*channel.read(), *h);
    }
    let channel = channel.clone();
    let wc = working_copy::memory::Memory::new();
    // The non-deleting variant, as the squash uses: this must leave the graph
    // the record then diffs exactly as the squash leaves it.
    output::output_repository_no_pending_(&wc, changes, txn, &channel, "", true, None, 1, 0)?;
    if std::env::var("FUZZ_VERBOSE").is_ok() {
        let mut buf = Vec::new();
        let _ = wc.read_file("file", &mut buf);
        eprintln!(
            "===== by hand, recording {:?} over {:?}",
            String::from_utf8_lossy(content),
            String::from_utf8_lossy(&buf)
        );
    }
    wc.write_file("file", Inode::ROOT)?.write_all(content)?;
    if let Ok(dir) = std::env::var("FUZZ_DOT") {
        crate::pristine::debug_to_file(&*txn.read(), &channel, format!("{dir}/before.dot"))?;
    }
    let h = record_all(&wc, changes, txn, &channel, "")?;
    if let Ok(dir) = std::env::var("FUZZ_DOT") {
        crate::pristine::debug_to_file(&*txn.read(), &channel, format!("{dir}/after.dot"))?;
    }
    let got = render(txn, &channel, changes)?;
    let faithful = got == (content.to_vec(), 0);
    if std::env::var("FUZZ_VERBOSE").is_ok() {
        eprintln!(
            "===== by hand: {} -> {:?} ({} conflicts)",
            h.to_base32(),
            String::from_utf8_lossy(&got.0),
            got.1
        );
        changes
            .get_change(&h)?
            .write(changes, Some(h), true, &mut std::io::stderr())?;
    }

    let fresh = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("by-hand-replay"))?;
    if let Ok(dir) = std::env::var("FUZZ_DOT") {
        // What a fresh replay of the survivors gives, to compare with the graph
        // the unrecords left (`before.dot`).
        let survivors_only = txn
            .write()
            .open_or_create_channel(&SmallString::from_str("by-hand-survivors"))?;
        for x in fuzz_valid_order(changes, survivors, false) {
            apply::apply_change_arc(changes, txn, &survivors_only, &x)?;
        }
        crate::pristine::debug_to_file(
            &*txn.read(),
            &survivors_only,
            format!("{dir}/survivors.dot"),
        )?;
    }
    let mut all = survivors.to_vec();
    all.push(h);
    for x in fuzz_valid_order(changes, &all, false) {
        apply::apply_change_arc(changes, txn, &fresh, &x)?;
    }
    if let Ok(dir) = std::env::var("FUZZ_DOT") {
        // The fresh replay of the survivors and the recorded change, to compare
        // with the channel after the record (`after.dot`).
        crate::pristine::debug_to_file(&*txn.read(), &fresh, format!("{dir}/replay.dot"))?;
    }
    let replayed = render(txn, &fresh, changes)?;
    if std::env::var("FUZZ_VERBOSE").is_ok() {
        eprintln!(
            "===== by hand, replayed: {:?} ({} conflicts)",
            String::from_utf8_lossy(&replayed.0),
            replayed.1
        );
    }
    Ok((faithful, replayed == got))
}

fn deps_satisfied(changes: &Store, applied: &[Hash], c: &Hash) -> bool {
    !applied.contains(c)
        && changes
            .get_change(c)
            .map(|ch| ch.dependencies.iter().all(|d| applied.contains(d)))
            .unwrap_or(false)
}

/// `c` and every change of `applied` depending on it, transitively.
fn closure(changes: &Store, applied: &[Hash], c: Hash, set: &mut Vec<Hash>) {
    if set.contains(&c) {
        return;
    }
    set.push(c);
    loop {
        let more: Vec<Hash> = applied
            .iter()
            .filter(|g| !set.contains(g))
            .filter(|g| {
                changes
                    .get_change(g)
                    .map(|gc| gc.dependencies.iter().any(|d| set.contains(d)))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if more.is_empty() {
            break;
        }
        set.extend(more);
    }
}

fn same_set(a: &[Hash], b: &[Hash]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x))
}

/// Walk `nsteps` steps of `seed`. With `by_hand_at`, stop at that step (a
/// squash) and return what [`record_by_hand`] says of it.
fn fuzz_squash_seed(
    seed: u64,
    nsteps: usize,
    verbose: bool,
    strict: bool,
    by_hand_at: Option<usize>,
) -> Result<Option<(bool, bool, (Vec<u8>, usize))>, anyhow::Error> {
    // Hashes, hence the conflict markers the walk may record as content, depend
    // on timestamps: give each seed its own deterministic clock.
    struct Clock;
    impl Drop for Clock {
        fn drop(&mut self) {
            crate::tests::fuzz_clock_clear()
        }
    }
    crate::tests::fuzz_clock_set(1_700_000_000_000_000_000 + (seed as i64) * 10_000_000);
    let _clock = Clock;
    if std::env::var("FUZZ_LOG_UNRECORD").is_ok() {
        log::set_max_level(log::LevelFilter::Off);
    }
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let changes = Store::new();

    let base = working_copy::memory::Memory::new();
    base.add_file("file", b"L0\nL1\nL2\nL3\n".to_vec());
    txn.write().add_file("file", 0)?;
    let c0 = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("c0"))?;
    // h0 adds the root, so it is never squashed.
    let h0 = record_all(&base, &changes, &txn, &c0, "")?;

    let mut chans = vec![(c0, vec![h0])];
    for i in 1..3 {
        let ch = txn
            .write()
            .fork(&chans[0].0, &SmallString::from_str(&format!("c{}", i)))?;
        chans.push((ch, vec![h0]));
    }
    let mut n_oracle = 0;
    let (mut accepted, mut refused) = (0, 0);

    let check_redundant = CHECK_REDUNDANT.get() || std::env::var("FUZZ_CHECK_REDUNDANT").is_ok();
    // At every step, check that no pseudo-edge orders the graph without a path
    // of dead vertices to back it (`FUZZ_CHECK_PSEUDO`).
    let check_pseudo = std::env::var("FUZZ_CHECK_PSEUDO").is_ok();
    // At every step, check that the channel's graph is edge for edge the one a
    // fresh replay of its changes builds (`FUZZ_CHECK_GRAPH`): localises the
    // step at which the graph stops being a function of its changes. Blocks
    // split at different positions in the two channels, which is benign, so
    // read its report rather than trusting it blindly.
    let check_graph = std::env::var("FUZZ_CHECK_GRAPH").is_ok();
    if std::env::var("FUZZ_REDUNDANT_STEP").is_ok() {
        log::set_max_level(log::LevelFilter::Off);
    }
    for step in 0..nsteps {
        if verbose {
            // Steps are logged once done: this tells which one failed.
            eprintln!("[{step}] start");
        }
        if check_redundant {
            let log_here = std::env::var("FUZZ_REDUNDANT_STEP")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                == Some(step);
            for ci in 0..chans.len() {
                if log_here {
                    eprintln!("===== c{ci} before del_redundant");
                    log::set_max_level(log::LevelFilter::Debug);
                }
                let before = render(&txn, &chans[ci].0, &changes)?;
                if log_here {
                    log::set_max_level(log::LevelFilter::Off);
                }
                if let Ok(dir) = std::env::var("FUZZ_DOT") {
                    crate::pristine::debug_to_file(
                        &*txn.read(),
                        &chans[ci].0,
                        format!("{dir}/redundant-before.dot"),
                    )?;
                }
                // Output no longer prunes them, so ask for the pruning here.
                let pos = {
                    let txn = txn.read();
                    let inode = crate::fs::find_inode(&*txn, "file")?;
                    *txn.get_inodes(&inode, None)?.unwrap()
                };
                prune_forward_edges(&txn, &chans[ci].0, pos)?;
                if log_here {
                    eprintln!("===== c{ci} after del_redundant");
                    log::set_max_level(log::LevelFilter::Debug);
                }
                let after = render(&txn, &chans[ci].0, &changes)?;
                if log_here {
                    log::set_max_level(log::LevelFilter::Off);
                }
                if let Ok(dir) = std::env::var("FUZZ_DOT")
                    && before != after
                {
                    crate::pristine::debug_to_file(
                        &*txn.read(),
                        &chans[ci].0,
                        format!("{dir}/redundant-after.dot"),
                    )?;
                }
                if before != after {
                    if verbose {
                        eprintln!(
                            "before = {:?} ({} conflicts)\nafter  = {:?} ({} conflicts)",
                            String::from_utf8_lossy(&before.0),
                            before.1,
                            String::from_utf8_lossy(&after.0),
                            after.1,
                        );
                    }
                    anyhow::bail!(
                        "seed {seed} step {step}: del_redundant changed the output of c{ci} \
                         ({} bytes/{} conflicts -> {} bytes/{} conflicts)",
                        before.0.len(),
                        before.1,
                        after.0.len(),
                        after.1,
                    );
                }
            }
        }
        match rng.random_range(0..100) {
            // Record a change on a channel.
            op if op < 40 => {
                let ci = rng.random_range(0..chans.len());
                let wc = working_copy::memory::Memory::new();
                output::output_repository_no_pending(
                    &wc,
                    &changes,
                    &txn,
                    &chans[ci].0,
                    "",
                    true,
                    None,
                    1,
                    0,
                )?;
                let mut buf = Vec::new();
                let _ = wc.read_file("file", &mut buf);
                let new = fuzz_mutate(&buf, &mut rng);
                if new == buf {
                    continue;
                }
                wc.write_file("file", Inode::ROOT)?.write_all(&new)?;
                let log_here = std::env::var("FUZZ_LOG_STEP")
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    == Some(step);
                if log_here {
                    env_logger::try_init().unwrap_or(());
                    log::set_max_level(log::LevelFilter::Debug);
                }
                let h = record_all(&wc, &changes, &txn, &chans[ci].0, "")?;
                if log_here {
                    log::set_max_level(log::LevelFilter::Off);
                }
                if let Ok(dir) = std::env::var("FUZZ_DOT") {
                    crate::pristine::debug_to_file(
                        &*txn.read(),
                        &chans[ci].0,
                        format!("{dir}/step{step}-c{ci}-rec.dot"),
                    )?;
                }
                chans[ci].1.push(h);
                if verbose {
                    eprintln!("[{step}] record c{ci} {}", h.to_base32());
                }
            }
            // Pull a known change into a channel, as `pull` does.
            op if op < 65 => {
                let mut all: Vec<Hash> = Vec::new();
                for (_, applied) in chans.iter() {
                    for h in applied {
                        if !all.contains(h) {
                            all.push(*h)
                        }
                    }
                }
                let c = all[rng.random_range(0..all.len())];
                let ci = rng.random_range(0..chans.len());
                if !deps_satisfied(&changes, &chans[ci].1, &c) || txn.read().is_superseded(&c)? {
                    continue;
                }
                let replaced = changes.get_change(&c)?.replaces_chain();
                match txn.write().unrecord_superseded(&changes, &chans[ci].0, &c) {
                    Ok(()) => {}
                    Err(crate::unrecord::UnrecordError::ChangeIsDependedUpon { .. }) => {
                        if verbose {
                            eprintln!("[{step}] pull {} into c{ci}: refused", c.to_base32());
                        }
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
                chans[ci].1.retain(|h| !replaced.contains(h));
                if !deps_satisfied(&changes, &chans[ci].1, &c) {
                    anyhow::bail!("seed {seed} step {step}: squash lost a dependency");
                }
                apply::apply_change_arc(&changes, &txn, &chans[ci].0, &c)?;
                chans[ci].1.push(c);
                if verbose {
                    eprintln!("[{step}] pull {} into c{ci}", c.to_base32());
                }
            }
            // Squash a dependency-closed set of changes of a channel.
            _ => {
                let ci = rng.random_range(0..chans.len());
                let cand: Vec<Hash> = chans[ci].1.iter().filter(|h| **h != h0).cloned().collect();
                if cand.is_empty() {
                    continue;
                }
                let mut set = Vec::new();
                for _ in 0..rng.random_range(1..=2) {
                    let c = cand[rng.random_range(0..cand.len())];
                    closure(&changes, &chans[ci].1, c, &mut set);
                }
                let before = render(&txn, &chans[ci].0, &changes)?;
                // Squashes of changes that cancelled out touch no file, and
                // conflicts in files a squash does not touch are fine.
                let mut touches = false;
                for h in set.iter() {
                    touches |= !changes.get_change(h)?.changes.is_empty();
                }
                let conflicted = touches && before.1 > 0;
                let applied_before = chans[ci].1.clone();
                // A deterministic timestamp too, for the same reason as the clock.
                let header = ChangeHeader {
                    message: format!("squash {step}"),
                    timestamp: jiff::Timestamp::from_nanosecond(
                        1_600_000_000_000_000_000 + (seed as i128) * 10_000_000 + step as i128,
                    )
                    .unwrap(),
                    ..ChangeHeader::default()
                };
                let ordered: Vec<Hash> = applied_before
                    .iter()
                    .filter(|h| set.contains(h))
                    .cloned()
                    .collect();
                if by_hand_at == Some(step) {
                    // Replaying up to a squash that was not equivalent: do its
                    // record by hand instead, on the very same graph.
                    let survivors: Vec<Hash> = applied_before
                        .iter()
                        .filter(|h| !set.contains(h))
                        .cloned()
                        .collect();
                    let (faithful, replays) = record_by_hand(
                        &txn,
                        &changes,
                        &chans[ci].0,
                        &ordered,
                        &survivors,
                        &before.0,
                    )?;
                    return Ok(Some((faithful, replays, before)));
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::squash::squash(&txn, &chans[ci].0, &changes, &set, header, false)
                }));
                let Ok(result) = result else {
                    // E.g. edges left introduced by a change removed from the
                    // pristine, which any record then panics on. Legitimate only
                    // if the same unrecords by hand do too.
                    let by_hand = std::panic::catch_unwind(|| {
                        fuzz_squash_seed(seed, step + 1, false, false, Some(step))
                    });
                    if by_hand.is_ok() {
                        anyhow::bail!("seed {seed} step {step}: squash panicked");
                    }
                    if verbose {
                        eprintln!("seed {seed} [{step}] squash panicked, as unrecord by hand");
                    }
                    return Ok(None);
                };
                match result {
                    Err(SquashError::Conflicts { .. }) => {
                        refused += 1;
                        if !conflicted {
                            anyhow::bail!("seed {seed} step {step}: refused without conflicts");
                        }
                        if render(&txn, &chans[ci].0, &changes)? != before {
                            anyhow::bail!(
                                "seed {seed} step {step}: a refused squash changed c{ci}"
                            );
                        }
                        if verbose {
                            eprintln!("[{step}] squash {} on c{ci}: conflicts", set.len());
                        }
                    }
                    Err(e) => {
                        anyhow::bail!("seed {seed} step {step}: squash failed: {:?}", e)
                    }
                    Ok(mut s) => {
                        accepted += 1;
                        if conflicted {
                            anyhow::bail!("seed {seed} step {step}: squashed over conflicts");
                        }
                        let hash = changes
                            .save_change(&mut s.change, |_, _| Ok::<_, anyhow::Error>(()))?;
                        match crate::squash::apply_squash(&txn, &chans[ci].0, &changes, &s, &hash) {
                            Ok(()) => {}
                            Err(SquashError::NotEquivalent { paths }) => {
                                if verbose {
                                    let after = render(&txn, &chans[ci].0, &changes)?;
                                    eprintln!(
                                        "[{step}] NOT EQUIVALENT\nbefore = {:?}\nafter  = {:?}",
                                        String::from_utf8_lossy(&before.0),
                                        String::from_utf8_lossy(&after.0),
                                    );
                                    for h in applied_before.iter().chain(std::iter::once(&hash)) {
                                        let squashed =
                                            if set.contains(h) { " (squashed)" } else { "" };
                                        eprintln!("===== {}{}", h.to_base32(), squashed);
                                        changes.get_change(h)?.write(
                                            &changes,
                                            Some(*h),
                                            true,
                                            &mut std::io::stderr(),
                                        )?;
                                    }
                                }
                                // A refusal is only legitimate if recording the
                                // same file by hand is not faithful either.
                                // The walk is deterministic: replay it up to this
                                // step, where nothing else perturbs the graph.
                                let (faithful, _, replayed_before) =
                                    fuzz_squash_seed(seed, step + 1, false, false, Some(step))?
                                        .ok_or_else(|| {
                                            anyhow::anyhow!(
                                                "seed {seed} step {step}: the replay did not reach the squash"
                                            )
                                        })?;
                                if replayed_before != before {
                                    anyhow::bail!(
                                        "seed {seed} step {step}: the walk is not deterministic"
                                    );
                                }
                                if faithful || strict {
                                    anyhow::bail!(
                                        "seed {seed} step {step}: squash not equivalent: {:?}",
                                        paths
                                    );
                                }
                                if verbose {
                                    eprintln!(
                                        "seed {seed} [{step}] squash on c{ci}: refused, record is not faithful here"
                                    );
                                }
                                // The transaction must be dropped: end this walk.
                                return Ok(None);
                            }
                            Err(e) => return Err(e.into()),
                        }
                        let after = render(&txn, &chans[ci].0, &changes)?;
                        if after != before && verbose {
                            for h in applied_before.iter().chain(std::iter::once(&hash)) {
                                let squashed = if set.contains(h) { " (squashed)" } else { "" };
                                eprintln!("===== {}{}", h.to_base32(), squashed);
                                changes.get_change(h)?.write(
                                    &changes,
                                    Some(*h),
                                    true,
                                    &mut std::io::stderr(),
                                )?;
                            }
                        }
                        if after != before {
                            anyhow::bail!(
                                "seed {seed} step {step}: squash changed c{ci}\n\
                                 before = {:?}\nafter  = {:?}",
                                String::from_utf8_lossy(&before.0),
                                String::from_utf8_lossy(&after.0),
                            );
                        }
                        chans[ci].1.retain(|h| !set.contains(h));
                        chans[ci].1.push(hash);
                        if verbose {
                            eprintln!(
                                "[{step}] squash {} changes on c{ci} -> {}",
                                set.len(),
                                hash.to_base32()
                            );
                        }

                        n_oracle += 1;
                        let fresh = txn.write().open_or_create_channel(&SmallString::from_str(
                            &format!("replay-{n_oracle}"),
                        ))?;
                        for h in fuzz_valid_order(&changes, &chans[ci].1, false) {
                            apply::apply_change_arc(&changes, &txn, &fresh, &h)?;
                        }
                        let replayed = render(&txn, &fresh, &changes)?;
                        if replayed != after
                            && let Ok(dir) = std::env::var("FUZZ_DOT")
                        {
                            // The channel the walk built, and the fresh replay of
                            // the very same changes: their difference is what the
                            // unrecords of the squash left behind.
                            crate::pristine::debug_to_file(
                                &*txn.read(),
                                &chans[ci].0,
                                format!("{dir}/walk-after.dot"),
                            )?;
                            crate::pristine::debug_to_file(
                                &*txn.read(),
                                &fresh,
                                format!("{dir}/walk-replay.dot"),
                            )?;
                        }
                        if replayed != after && check_pseudo {
                            let unjustified = crate::pristine::check_pseudo_justified(
                                &*txn.read(),
                                &*chans[ci].0.read(),
                            );
                            if !unjustified.is_empty() {
                                anyhow::bail!(
                                    "seed {seed} step {step}: unjustified pseudo-edges on c{ci}: {:?}",
                                    unjustified
                                );
                            }
                        }
                        if replayed != after {
                            // Legitimate only if the same record, done by hand
                            // after the same unrecords, does not replay either.
                            let (_, by_hand_replays, replayed_before) =
                                fuzz_squash_seed(seed, step + 1, false, false, Some(step))?
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("seed {seed} step {step}: replay diverged")
                                    })?;
                            if replayed_before != before {
                                anyhow::bail!(
                                    "seed {seed} step {step}: the walk is not deterministic"
                                );
                            }
                            if !by_hand_replays {
                                if verbose {
                                    eprintln!(
                                        "seed {seed} [{step}] squash on c{ci}: replaying differs, as for a record by hand"
                                    );
                                }
                                return Ok(None);
                            }
                            anyhow::bail!(
                                "seed {seed} step {step}: replaying c{ci} differs\n\
                                 channel  = {:?} ({} conflicts)\nreplayed = {:?} ({} conflicts)",
                                String::from_utf8_lossy(&after.0),
                                after.1,
                                String::from_utf8_lossy(&replayed.0),
                                replayed.1,
                            );
                        }

                        for cj in 0..chans.len() {
                            if cj == ci || !same_set(&chans[cj].1, &applied_before) {
                                continue;
                            }
                            txn.write()
                                .unrecord_superseded(&changes, &chans[cj].0, &hash)?;
                            apply::apply_change_arc(&changes, &txn, &chans[cj].0, &hash)?;
                            chans[cj].1 = chans[ci].1.clone();
                            if render(&txn, &chans[cj].0, &changes)? != after {
                                anyhow::bail!(
                                    "seed {seed} step {step}: c{cj} pulled the squash differently"
                                );
                            }
                        }
                    }
                }
            }
        }
        if check_graph {
            for ci in 0..chans.len() {
                n_oracle += 1;
                let fresh = txn
                    .write()
                    .open_or_create_channel(&SmallString::from_str(&format!("graph-{n_oracle}")))?;
                for h in fuzz_valid_order(&changes, &chans[ci].1, false) {
                    apply::apply_change_arc(&changes, &txn, &fresh, &h)?;
                }
                let walk = graph_lines(&*txn.read(), &chans[ci].0);
                let replayed = graph_lines(&*txn.read(), &fresh);
                if let Ok(dir) = std::env::var("FUZZ_DOT") {
                    crate::pristine::debug_to_file(
                        &*txn.read(),
                        &chans[ci].0,
                        format!("{dir}/step{step}-c{ci}.dot"),
                    )?;
                    if walk != replayed {
                        crate::pristine::debug_to_file(
                            &*txn.read(),
                            &fresh,
                            format!("{dir}/step{step}-c{ci}-replay.dot"),
                        )?;
                    }
                }
                if walk != replayed {
                    let missing: Vec<_> = replayed.iter().filter(|l| !walk.contains(l)).collect();
                    let extra: Vec<_> = walk.iter().filter(|l| !replayed.contains(l)).collect();
                    anyhow::bail!(
                        "seed {seed} step {step}: c{ci} differs from its replay\n\
                         missing = {:#?}\nextra = {:#?}",
                        missing,
                        extra
                    );
                }
            }
        }
        if check_pseudo {
            for ci in 0..chans.len() {
                let unjustified =
                    crate::pristine::check_pseudo_justified(&*txn.read(), &*chans[ci].0.read());
                if !unjustified.is_empty() {
                    anyhow::bail!(
                        "seed {seed} step {step}: unjustified pseudo-edges on c{ci}: {:?}",
                        unjustified
                    );
                }
            }
        }
    }
    if verbose {
        eprintln!("seed {seed}: {accepted} squashes accepted, {refused} refused");
    }
    Ok(None)
}

/// With `strict`, a squash refused because record is not faithful is an error
/// even if recording by hand is not faithful either.
fn fuzz_squash_run(start: u64, seeds: u64, steps: usize, strict: bool) {
    env_logger::try_init().unwrap_or(());
    let verbose = std::env::var("FUZZ_VERBOSE").is_ok();
    for seed in start..start + seeds {
        if let Err(e) = fuzz_squash_seed(seed, steps, verbose, strict, None) {
            // The `FUZZ_CHECK_REDUNDANT` probe outputs (hence deletes redundant
            // pseudo-edges) on its own: the walk is no longer the reference one,
            // so only the probe's own findings count.
            if std::env::var("FUZZ_CHECK_REDUNDANT").is_ok() {
                if format!("{e}").contains("del_redundant changed") {
                    eprintln!("REDUNDANT {e}");
                } else if verbose {
                    eprintln!("seed {seed}: skipped ({e})");
                }
                continue;
            }
            // A long run reporting every failing seed instead of stopping at the
            // first one.
            if std::env::var("FUZZ_COLLECT").is_ok() {
                eprintln!("FAILURE {}", format!("{e}").lines().next().unwrap_or(""));
                continue;
            }
            panic!("{}", e)
        }
    }
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[test]
fn fuzz_squash() {
    fuzz_squash_run(0, 24, 60, false);
}

/// A file holding a line recorded from a conflict marker, squashed while the
/// version it is diffed against has that very conflict: the line used to be
/// taken for the marker, and the conflict for resolved.
#[test]
fn fuzz_squash_marker_line() {
    fuzz_squash_run(1001, 1, 36, true);
}

/// A resolution of zombies where an empty vertex (the resolution of an order
/// conflict) shares its position with the next line: it used to stay a zombie,
/// leaving an empty conflict.
#[test]
fn fuzz_squash_empty_zombie() {
    fuzz_squash_run(1071, 1, 120, true);
}

/// Lines deleted right after an empty vertex at the same position: the down
/// context of an insertion above them used to be the end of the next vertex,
/// which is not a position of the graph (`Block error` when applying).
#[test]
fn fuzz_squash_down_context_after_empty_vertex() {
    fuzz_squash_run(2259, 1, 138, false);
}

/// Lines looking like the markers of a conflict that the squashed change does
/// create: the same bytes, but a conflict. The squash must be refused.
#[test]
fn fuzz_squash_marker_lookalike_conflict() {
    fuzz_squash_run(1121, 1, 56, false);
}

/// Re-adding an alive edge whose target has parents deleted by a change
/// unknown to the applied one marks it deleted by that change too. The mark
/// used to be introduced by the change of the deleted *vertex*, which never
/// deleted anything there: unrecording it (as squashing it does) left the edge
/// behind (`Dependency missing` when replaying).
#[test]
fn fuzz_squash_zombie_mark_introducer() {
    fuzz_squash_run(2863, 1, 141, false);
}

/// Two conflicts shown one after the other, whose sides are not ordered in the
/// graph: deleting from the end of the first one to the start of the second one
/// joins lines that must be ordered by the record (`SolveOrderConflict`), else
/// the order conflict comes back.
#[test]
fn fuzz_squash_join_consecutive_conflicts() {
    fuzz_squash_run(1382, 1, 110, true);
    fuzz_squash_run(1752, 1, 132, true);
}

/// Lines added at the end of a file that ends with an empty vertex (the
/// resolution of an order conflict, after its last line): without that vertex
/// for down context, they are in an order conflict with it, i.e. a conflict
/// with an empty side.
#[test]
fn fuzz_squash_empty_vertex_at_end() {
    fuzz_squash_run(1565, 1, 81, true);
    fuzz_squash_run(1858, 1, 136, true);
}

/// Unrecording a change that deleted the edge out of a dead vertex brings that
/// edge back: its alive ancestor must be reconnected to its now alive child. The
/// reconnection used to be skipped as redundant, the child being "reachable"
/// through the dead vertex, which orders nothing: the record then ordered them
/// the other way, and the pseudo-edge put back later made a cycle.
#[test]
fn fuzz_squash_reconnect_through_dead_vertex() {
    fuzz_squash_run(2782, 1, 64, true);
}

/// After an unrecord, an alive vertex whose only way down is a dead region
/// visited earlier from elsewhere: the repair knew no alive exit below the dead
/// vertex it entered through, and did not reconnect it (the fresh replay, which
/// visits in another order, does).
#[test]
fn fuzz_squash_exits_through_dead_region() {
    fuzz_squash_run(2113, 1, 121, true);
    fuzz_squash_run(2109, 1, 112, true);
    fuzz_squash_run(2577, 1, 75, true);
    fuzz_squash_run(2796, 1, 131, true);
}

/// Outputting a channel deletes the pseudo-edges the conflict DFS found
/// redundant, and Tarjan's numbering — hence the conflict tree — depended on
/// them: the same channel then output twice gave two different files, and a
/// channel and a fresh replay of its changes differed by those edges alone.
#[test]
fn fuzz_squash_output_after_del_redundant() {
    CHECK_REDUNDANT.set(true);
    // Cyclic conflict whose two vertices swapped (1998), then conflict trees
    // partitioned differently (the others).
    for (seed, steps) in [
        (1998, 148),
        (2731, 96),
        (2470, 136),
        (1325, 111),
        (1924, 53),
        (2671, 98),
    ] {
        fuzz_squash_run(seed, 1, steps, false)
    }
}

/// Output used to *delete* the pseudo-edges its conflict DFS found redundant,
/// and which ones it found depends on the DFS order: a channel's graph then
/// depended on when it had been output, not only on its changes. Here the walk
/// output c1 at step 26 while an alive vertex still short-circuited a dead
/// region; the shortcut over that region was dropped and never came back, and
/// at step 131 the squash rendered a line in the wrong conflict side.
#[test]
fn fuzz_squash_output_keeps_redundant_edges() {
    fuzz_squash_run(1248, 1, 132, false);
}

/// Replay `FUZZ_SQUASH_START` up to each step from `FUZZ_SQUASH_BY_HAND_FROM`
/// to `FUZZ_SQUASH_BY_HAND_TO`, and do the record of the squash at that step by
/// hand instead (to tell squash bugs from record or unrecord ones).
#[test]
#[ignore]
fn fuzz_squash_by_hand() {
    env_logger::try_init().unwrap_or(());
    let seed = env_or("FUZZ_SQUASH_START", 0);
    for at in env_or("FUZZ_SQUASH_BY_HAND_FROM", 0)..=env_or("FUZZ_SQUASH_BY_HAND_TO", 0) {
        let at = at as usize;
        match fuzz_squash_seed(seed, at + 1, false, false, Some(at)) {
            Ok(Some((faithful, replays, _))) => {
                eprintln!("step {at}: by hand faithful = {faithful}, replays = {replays}");
                break;
            }
            Ok(None) => eprintln!("step {at}: not a squash"),
            Err(e) => panic!("step {at}: {}", e),
        }
    }
}

#[test]
#[ignore]
fn fuzz_squash_long() {
    fuzz_squash_run(
        env_or("FUZZ_SQUASH_START", 1000),
        env_or("FUZZ_SQUASH_SEEDS", 500),
        env_or("FUZZ_SQUASH_STEPS", 200) as usize,
        std::env::var("FUZZ_STRICT").is_ok(),
    );
}
