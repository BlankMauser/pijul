//! Shared answers for the bounded forward probes of the minimal-exiting step.
//!
//! `forward_reachable_within(r, targets)` walks forward from `r` for at most
//! `PROBE_BUDGET` pops and reports the targets it scans. The walk itself does
//! not depend on the targets: they only let it stop early once every one of
//! them is found, and then its answer is all of them either way. So its answer
//! is `targets ∩ scanned(r)`, where `scanned(r)` is every child the full
//! bounded walk scans. The same exit vertex is probed for many dead vertices
//! (in the HareDream ledger, 43 652 probes from 195 distinct vertices), so the
//! walk is done once per vertex and kept.
//!
//! A walk is also kept half done: a question that only needs to know whether
//! the walk scans one vertex (`any_scans`) runs it until it does, and a later
//! question resumes it where it stopped. Up to any pop, a resumed walk is the
//! walk itself: it has read the same adjacency and aliveness, so its stack and
//! visited set are the ones a fresh walk would have there.
//!
//! The walk reads the adjacency of the vertices it expands, and aliveness,
//! which a pseudo-edge never changes. An entry is dropped when an edge is
//! inserted from a vertex its walk expanded; other insertions do not change it
//! (a vertex expanded after the insertion reads the new adjacency, as a fresh
//! walk would).
use super::PROBE_BUDGET;
use super::dead_context::DeadContext;
use crate::pristine::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashSet;
use std::hash::BuildHasher;

/// Stored vertices beyond which the memo starts over (only costs recomputing).
const MAX_STORED: usize = 1 << 23;

#[derive(Default)]
pub(super) struct ScanMemo {
    walks: FxHashMap<Vertex<ChangeId>, Walk>,
    /// Expanded vertex -> walks that expanded it. May hold stale sources.
    expanded_by: FxHashMap<Vertex<ChangeId>, Vec<Vertex<ChangeId>>>,
    stored: usize,
}

/// One walk of `forward_reachable_within`, possibly half done.
struct Walk {
    budget: usize,
    visited: FxHashSet<Vertex<ChangeId>>,
    stack: Vec<Vertex<ChangeId>>,
    scanned: FxHashSet<Vertex<ChangeId>>,
}

impl Walk {
    fn done(&self) -> bool {
        self.budget == 0 || self.stack.is_empty()
    }
}

impl ScanMemo {
    /// Same answer as `forward_reachable_within(from, targets, out)`.
    pub(super) fn reachable_within<T: GraphTxnT, S: BuildHasher, S2: BuildHasher>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        from: Vertex<ChangeId>,
        targets: &HashSet<Vertex<ChangeId>, S>,
        out: &mut HashSet<Vertex<ChangeId>, S2>,
        context: &mut DeadContext,
    ) -> Result<(), BlockError<T::GraphError>> {
        out.clear();
        if !self.walks.contains_key(&from) {
            self.trim();
        }
        self.start(from);
        while !self.walks[&from].done() {
            self.step(txn, channel, from, None, context)?;
        }
        let scanned = &self.walks[&from].scanned;
        for &t in targets.iter() {
            if t != from && scanned.contains(&t) {
                out.insert(t);
            }
        }
        Ok(())
    }

    /// Does the walk from any of `froms` scan `target`? Same answer as
    /// `reachable_within(from, {target})` for each `from != target`, then
    /// "any". The walks run side by side, one pop each in turn, until one of
    /// them scans `target` or all are done, and are kept where they stopped.
    pub(super) fn any_scans<T: GraphTxnT>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        froms: impl Iterator<Item = Vertex<ChangeId>>,
        target: Vertex<ChangeId>,
        context: &mut DeadContext,
    ) -> Result<bool, BlockError<T::GraphError>> {
        self.trim();
        let mut running = Vec::new();
        for from in froms {
            if from == target {
                continue;
            }
            self.start(from);
            let w = &self.walks[&from];
            if w.scanned.contains(&target) {
                return Ok(true);
            }
            if !w.done() {
                running.push(from);
            }
        }
        while !running.is_empty() {
            let mut i = 0;
            while i < running.len() {
                let from = running[i];
                if self.step(txn, channel, from, Some(target), context)? {
                    return Ok(true);
                }
                if self.walks[&from].done() {
                    running.swap_remove(i);
                } else {
                    i += 1;
                }
            }
        }
        Ok(false)
    }

    /// An edge from `source` was inserted: drop the walks that read its
    /// adjacency.
    pub(super) fn invalidate(&mut self, source: Vertex<ChangeId>) {
        if let Some(walks) = self.expanded_by.remove(&source) {
            for w in walks {
                if let Some(s) = self.walks.remove(&w) {
                    self.stored -= s.scanned.len() + PROBE_BUDGET;
                }
            }
        }
    }

    /// Start over when the stored walks hold too much. Only called before a
    /// question starts its walks, so that none of them disappears under it.
    fn trim(&mut self) {
        if self.stored > MAX_STORED {
            self.walks.clear();
            self.expanded_by.clear();
            self.stored = 0;
        }
    }

    fn start(&mut self, from: Vertex<ChangeId>) {
        if self.walks.contains_key(&from) {
            return;
        }
        let mut visited = FxHashSet::default();
        visited.insert(from);
        self.stored += PROBE_BUDGET;
        self.walks.insert(
            from,
            Walk {
                budget: PROBE_BUDGET,
                visited,
                stack: vec![from],
                scanned: FxHashSet::default(),
            },
        );
    }

    /// One pop of the walk from `from` (which is not done), expanding the
    /// popped vertex completely. Returns whether that expansion scanned
    /// `target`.
    fn step<T: GraphTxnT>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        from: Vertex<ChangeId>,
        target: Option<Vertex<ChangeId>>,
        context: &mut DeadContext,
    ) -> Result<bool, BlockError<T::GraphError>> {
        let w = self.walks.get_mut(&from).unwrap();
        let v = w.stack.pop().unwrap();
        w.budget -= 1;
        if v != from && !context.is_alive(txn, channel, v)? {
            return Ok(false);
        }
        let mut hit = false;
        let before = w.scanned.len();
        for &c in context.forward_edges(txn, channel, v)? {
            hit |= Some(c) == target;
            w.scanned.insert(c);
            if w.visited.insert(c) {
                w.stack.push(c)
            }
        }
        self.stored += w.scanned.len() - before;
        self.expanded_by.entry(v).or_default().push(from);
        Ok(hit)
    }
}
