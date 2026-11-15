//! The repair shortcuts (`apply::repair_shortcuts`: skipped reductions,
//! undeferred direct bridges, the lazy `DeadExits` snapshot, the sweep read
//! from the zombie walk) must build the very graphs the full repair builds.
//! Each test runs the same seeded history twice, with and without them, and
//! compares every channel's graph, edge by edge, and its output and conflicts,
//! after every step.

use super::squash_fuzz::{TRACE, fuzz_squash_seed};
use super::unrecord::fuzz_mutate;
use super::*;
use crate::apply::FULL_REPAIR;
use crate::working_copy::{WorkingCopy, WorkingCopyRead};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha20Rng;
use std::io::Write;

type Store = changestore::memory::Memory;

/// Restores the shortcuts and drops the trace, even when a run panics.
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        FULL_REPAIR.set(false);
        TRACE.with(|t| *t.borrow_mut() = None);
        crate::tests::fuzz_clock_clear();
    }
}

fn first_difference(a: &[String], b: &[String]) -> String {
    if let Some(i) = (0..a.len().min(b.len())).find(|&i| a[i] != b[i]) {
        let la: Vec<_> = a[i].lines().collect();
        let lb: Vec<_> = b[i].lines().collect();
        let only_a: Vec<_> = la.iter().filter(|l| !lb.contains(l)).collect();
        let only_b: Vec<_> = lb.iter().filter(|l| !la.contains(l)).collect();
        format!("entry {i}: with shortcuts only {only_a:#?}\nfull repair only {only_b:#?}")
    } else {
        format!("lengths {} and {}", a.len(), b.len())
    }
}

/// The squash fuzz walk (record, pull, squash across three channels of one
/// file, with conflicts and zombies), traced with and without the shortcuts.
fn fuzz_walk(seed: u64, steps: usize, full: bool) -> (Vec<String>, String) {
    let _reset = Reset;
    FULL_REPAIR.set(full);
    TRACE.with(|t| *t.borrow_mut() = Some(Vec::new()));
    let result = fuzz_squash_seed(seed, steps, false, false, None);
    let trace = TRACE.with(|t| t.borrow_mut().take().unwrap());
    (trace, format!("{:?}", result.map(|_| ())))
}

#[test]
fn shortcuts_build_the_full_repair_graphs_on_fuzz_walks() {
    let seeds = std::env::var("REPAIR_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24u64);
    let steps = std::env::var("REPAIR_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60usize);
    for seed in 0..seeds {
        let (fast, fast_result) = fuzz_walk(seed, steps, false);
        let (full, full_result) = fuzz_walk(seed, steps, true);
        assert_eq!(fast_result, full_result, "seed {seed}");
        assert!(
            fast == full,
            "seed {seed}: {}",
            first_difference(&fast, &full)
        );
    }
}

/// The sorted dot lines of `channel`'s graph and its output.
fn snapshot<T>(txn: &ArcTxn<T>, channel: &ChannelRef<T>, changes: &Store) -> String
where
    T: MutTxnT + crate::pristine::GraphIter + Send + Sync + 'static,
    T::Channel: Send + Sync,
{
    let mut buf = Vec::new();
    {
        let t = txn.read();
        let ch = channel.read();
        crate::pristine::debug(&*t, t.graph(&*ch), &mut buf).unwrap();
    }
    let mut lines: Vec<String> = String::from_utf8_lossy(&buf)
        .lines()
        .map(|s| s.to_string())
        .collect();
    lines.sort();
    txn.write().begin_scratch_tree().unwrap();
    let view = working_copy::memory::Memory::new();
    let conflicts =
        output::output_repository_no_pending_(&view, changes, txn, channel, "", true, None, 1, 0)
            .unwrap();
    txn.write().end_scratch_tree().unwrap();
    let mut out = Vec::new();
    let _ = view.read_file("file", &mut out);
    format!(
        "{}\n--\n{}\n{} conflicts",
        lines.join("\n"),
        String::from_utf8_lossy(&out),
        conflicts.len()
    )
}

/// Agents edit forks of one long file, a few lines per push, and every push
/// lands on a trunk in a seeded order; then the trunk unrecords its last
/// changes and lands them again. Long files reach the `DeadExits` snapshot
/// and many dead regions per walk, which the three-line fuzz file does not.
fn agents_landing(seed: u64, full: bool) -> Result<Vec<String>, anyhow::Error> {
    let _reset = Reset;
    FULL_REPAIR.set(full);
    crate::tests::fuzz_clock_set(1_650_000_000_000_000_000 + (seed as i64) * 10_000_000);
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let env = pristine::sanakirja::Pristine::new_anon()?;
    let txn = env.arc_txn_begin().unwrap();
    let changes = Store::new();
    let mut trace = Vec::new();

    let lines = rng.random_range(40..200);
    let mut base = Vec::new();
    for i in 0..lines {
        writeln!(base, "line {i}")?;
    }
    let wc = working_copy::memory::Memory::new();
    wc.add_file("file", base);
    txn.write().add_file("file", 0)?;
    let trunk = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("trunk"))?;
    let h0 = record_all(&wc, &changes, &txn, &trunk, "")?;

    // Each agent records its pushes on its own fork.
    let agents = rng.random_range(1..6);
    let mut pushes: Vec<Vec<Hash>> = Vec::new();
    for a in 0..agents {
        let ch = txn
            .write()
            .fork(&trunk, &SmallString::from_str(&format!("agent{a}")))?;
        let wc = working_copy::memory::Memory::new();
        output::output_repository_no_pending(&wc, &changes, &txn, &ch, "", true, None, 1, 0)?;
        let mut hs = Vec::new();
        for _ in 0..rng.random_range(2..12) {
            let mut buf = Vec::new();
            wc.read_file("file", &mut buf)?;
            // A few mutations in one window of the file: agents overlap
            // sometimes, which makes conflicts and zombies on the trunk.
            let text: Vec<&[u8]> = buf.split_inclusive(|&b| b == b'\n').collect();
            let n = text.len().max(1);
            let at = rng.random_range(0..n);
            let len = rng.random_range(1..8).min(n - at);
            let window: Vec<u8> = text[at..at + len].concat();
            let mut new = text[..at].concat();
            new.extend(fuzz_mutate(&window, &mut rng));
            new.extend(text[at + len..].concat());
            if new == buf {
                continue;
            }
            wc.write_file("file", Inode::ROOT)?.write_all(&new)?;
            hs.push(record_all(&wc, &changes, &txn, &ch, "")?);
        }
        pushes.push(hs);
    }

    // Land them, each agent's in its own order, interleaved.
    let mut next = vec![0; pushes.len()];
    let mut landed = vec![h0];
    let mut rollbacks = 0i128;
    loop {
        let open: Vec<usize> = (0..pushes.len())
            .filter(|&a| next[a] < pushes[a].len())
            .collect();
        if open.is_empty() {
            break;
        }
        let a = open[rng.random_range(0..open.len())];
        let h = pushes[a][next[a]];
        next[a] += 1;
        apply::apply_change_arc(&changes, &txn, &trunk, &h)?;
        landed.push(h);
        trace.push(snapshot(&txn, &trunk, &changes));
        // Sometimes roll back a landed change: its inverse brings deleted
        // lines back to life, which leaves pseudo-edges over them without a
        // dead path (what the sweep deletes).
        if rng.random_range(0..4) == 0 && landed.len() > 1 {
            let target = landed[rng.random_range(1..landed.len())];
            rollbacks += 1;
            let mut inv = changes.get_change(&target)?.inverse(
                &target,
                crate::change::ChangeHeader {
                    message: format!("rollback {rollbacks}"),
                    timestamp: jiff::Timestamp::from_nanosecond(
                        1_640_000_000_000_000_000 + (seed as i128) * 10_000_000 + rollbacks,
                    )
                    .unwrap(),
                    ..Default::default()
                },
                Vec::new(),
            );
            let hi = changes.save_change(&mut inv, |_, _| Ok::<_, anyhow::Error>(()))?;
            match apply::apply_change_arc(&changes, &txn, &trunk, &hi) {
                Ok(_) => {
                    landed.push(hi);
                    trace.push(snapshot(&txn, &trunk, &changes));
                }
                Err(e) => trace.push(format!("rollback refused: {e}")),
            }
        }
    }

    // Unrecord the last few (most recent first), then land them again.
    let k = rng.random_range(0..landed.len().min(6));
    let tail: Vec<Hash> = landed[landed.len() - k..].to_vec();
    for h in tail.iter().rev() {
        crate::unrecord::unrecord(
            &mut *txn.write(),
            &trunk,
            &changes,
            h,
            0,
            &mut Default::default(),
        )?;
        trace.push(snapshot(&txn, &trunk, &changes));
    }
    for h in tail.iter() {
        apply::apply_change_arc(&changes, &txn, &trunk, h)?;
        trace.push(snapshot(&txn, &trunk, &changes));
    }

    // A fresh replay of the trunk's changes, as a clone does.
    let replay = txn
        .write()
        .open_or_create_channel(&SmallString::from_str("replay"))?;
    for h in landed.iter() {
        apply::apply_change_arc(&changes, &txn, &replay, h)?;
        trace.push(snapshot(&txn, &replay, &changes));
    }
    Ok(trace)
}

#[test]
fn shortcuts_build_the_full_repair_graphs_on_agents_landing() {
    let seeds = std::env::var("REPAIR_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16u64);
    for seed in 0..seeds {
        let fast = agents_landing(seed, false).map_err(|e| e.to_string());
        let full = agents_landing(seed, true).map_err(|e| e.to_string());
        match (fast, full) {
            (Ok(fast), Ok(full)) => assert!(
                fast == full,
                "seed {seed}: {}",
                first_difference(&fast, &full)
            ),
            (fast, full) => assert_eq!(fast.err(), full.err(), "seed {seed}"),
        }
    }
}
