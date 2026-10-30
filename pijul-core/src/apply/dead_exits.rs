//! Snapshot the alive frontier of dead regions once per zombie repair.
//!
//! Repeating a graph search for every revisited dead vertex is quadratic in a
//! long deleted region. Pseudo-edges from alive sources do not change these
//! frontiers, since a search stops on reaching an alive vertex. The caller
//! discards the snapshot if an inserted edge could extend a dead frontier.
//! An edge whose destination frontier is already covered changes no snapshot.
use crate::pristine::*;
use rustc_hash::FxHashMap as HashMap;
use std::collections::HashSet;
use std::collections::{BTreeSet, VecDeque};
use std::hash::BuildHasher;

pub(super) struct DeadExits {
    dead: HashMap<Vertex<ChangeId>, usize>,
    alive: Vec<Vertex<ChangeId>>,
    exits: Vec<Vec<u64>>,
}

impl DeadExits {
    pub(super) fn new<T: GraphTxnT>(
        txn: &T,
        graph: &T::Graph,
        vertices: &BTreeSet<Vertex<ChangeId>>,
    ) -> Result<Option<Self>, BlockError<T::GraphError>> {
        let mut dead = HashMap::default();
        let mut alive = Vec::new();
        let mut alive_ids = HashMap::default();
        for &v in vertices {
            if is_alive(txn, graph, &v)? {
                alive_ids.insert(v, alive.len());
                alive.push(v);
            } else {
                dead.insert(v, dead.len());
            }
        }
        let words = alive.len().div_ceil(64);
        // Bound the dense matrix to 64 MiB; oversized files retain the exact uncached
        // search, not a truncated frontier (which would lose reconnections).
        if dead
            .len()
            .checked_mul(words)
            .is_none_or(|n| n > 8 * 1024 * 1024)
        {
            return Ok(None);
        }
        let mut exits = vec![vec![0u64; words]; dead.len()];
        let mut children = vec![Vec::new(); dead.len()];
        for (&v, &i) in &dead {
            for e in iter_adjacent(
                txn,
                graph,
                v,
                EdgeFlags::empty(),
                EdgeFlags::all() - EdgeFlags::PARENT - EdgeFlags::FOLDER,
            )? {
                let e = e?;
                if e.flag().contains(EdgeFlags::PARENT) {
                    continue;
                }
                let c = txn.find_block(graph, e.dest())?;
                if let Some(&j) = dead.get(c) {
                    children[i].push(j);
                } else if let Some(&j) = alive_ids.get(c) {
                    exits[i][j / 64] |= 1 << (j % 64);
                } else {
                    // A caller supplied only part of the reachable graph.
                    return Ok(None);
                }
            }
            children[i].sort_unstable();
            children[i].dedup();
        }
        propagate(&children, &mut exits);
        Ok(Some(Self { dead, alive, exits }))
    }

    pub(super) fn invalidate_after_insert(
        snapshot: &mut Option<Self>,
        from: Vertex<ChangeId>,
        to: Vertex<ChangeId>,
    ) {
        if snapshot
            .as_ref()
            .is_some_and(|s| !s.preserves_frontiers(from, to))
        {
            log::debug!(target: "pijul_core::apply::repair", "frontier snapshot invalidated: {:?} -> {:?}", from, to);
            *snapshot = None;
        }
    }

    fn preserves_frontiers(&self, from: Vertex<ChangeId>, to: Vertex<ChangeId>) -> bool {
        // `alive` was populated in the input BTreeSet order.
        if self.alive.binary_search(&from).is_ok() {
            return true;
        }
        let Some(&i) = self.dead.get(&from) else {
            return false;
        };
        if let Some(&j) = self.dead.get(&to) {
            // The new edge reaches no alive exit that was not already
            // reachable from `from`, so no ancestor's frontier changes either.
            self.exits[j]
                .iter()
                .zip(&self.exits[i])
                .all(|(&r, &l)| r & !l == 0)
        } else if let Ok(j) = self.alive.binary_search(&to) {
            self.exits[i][j / 64] & (1 << (j % 64)) != 0
        } else {
            false
        }
    }

    pub(super) fn extend<S: BuildHasher>(
        &self,
        d: Vertex<ChangeId>,
        out: &mut HashSet<Vertex<ChangeId>, S>,
    ) {
        if let Some(&i) = self.dead.get(&d) {
            for (word, &bits) in self.exits[i].iter().enumerate() {
                let mut bits = bits;
                while bits != 0 {
                    let bit = bits.trailing_zeros() as usize;
                    out.insert(self.alive[word * 64 + bit]);
                    bits &= bits - 1;
                }
            }
        }
    }
}

// Children-before-parents scheduling finishes an acyclic region in one pass.
// The worklist also handles cycles: only newly discovered exits propagate, and
// the finite bitsets converge to the least fixed point (exact reachability).
fn propagate(children: &[Vec<usize>], exits: &mut [Vec<u64>]) -> usize {
    let mut parents = vec![Vec::new(); children.len()];
    for (i, cs) in children.iter().enumerate() {
        for &c in cs {
            parents[c].push(i);
        }
    }
    let mut seen = vec![false; children.len()];
    let mut order = Vec::with_capacity(children.len());
    for root in 0..children.len() {
        let mut stack = vec![(root, false)];
        while let Some((i, post)) = stack.pop() {
            if post {
                order.push(i);
            } else if !std::mem::replace(&mut seen[i], true) {
                stack.push((i, true));
                stack.extend(children[i].iter().map(|&c| (c, false)));
            }
        }
    }
    let mut queue: VecDeque<_> = order.into();
    let mut queued = vec![true; children.len()];
    let mut scans = 0;
    while let Some(i) = queue.pop_front() {
        queued[i] = false;
        for &p in &parents[i] {
            if p == i {
                continue;
            }
            scans += 1;
            let (src, dst) = if i < p {
                let (lo, hi) = exits.split_at_mut(p);
                (&lo[i], &mut hi[0])
            } else {
                let (lo, hi) = exits.split_at_mut(i);
                (&hi[0], &mut lo[p])
            };
            let mut changed = false;
            for (&s, d) in src.iter().zip(dst) {
                let union = *d | s;
                changed |= union != *d;
                *d = union;
            }
            if changed && !std::mem::replace(&mut queued[p], true) {
                queue.push_back(p);
            }
        }
    }
    scans
}

#[cfg(test)]
mod tests {
    use super::propagate;

    #[test]
    fn deleted_chain_shares_its_frontier() {
        let n = 10_000;
        let mut children = vec![Vec::new(); n];
        for (i, cs) in children.iter_mut().enumerate().take(n - 1) {
            cs.push(i + 1);
        }
        let mut exits = vec![vec![0; 2]; n];
        exits[n - 1] = vec![0x55, 0xaa];
        assert_eq!(propagate(&children, &mut exits), n - 1);
        assert!(exits.iter().all(|e| e == &[0x55, 0xaa]));
    }

    #[test]
    fn cyclic_dead_region_has_all_and_only_reachable_exits() {
        let children = vec![vec![1], vec![2, 3], vec![0], vec![], vec![4]];
        let mut exits = vec![vec![1], vec![0], vec![2], vec![4], vec![8]];
        propagate(&children, &mut exits);
        assert_eq!(exits, vec![vec![7], vec![7], vec![7], vec![4], vec![8]]);
    }
}
