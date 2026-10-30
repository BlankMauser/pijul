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
//! The walk reads the adjacency of the vertices it expands, and aliveness,
//! which a pseudo-edge never changes. An entry is dropped when an edge is
//! inserted from a vertex its walk expanded; other insertions do not change it.
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
    scanned: FxHashMap<Vertex<ChangeId>, FxHashSet<Vertex<ChangeId>>>,
    /// Expanded vertex -> walks that expanded it. May hold stale sources.
    expanded_by: FxHashMap<Vertex<ChangeId>, Vec<Vertex<ChangeId>>>,
    stored: usize,
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
        if !self.scanned.contains_key(&from) {
            if self.stored > MAX_STORED {
                self.scanned.clear();
                self.expanded_by.clear();
                self.stored = 0;
            }
            self.walk(txn, channel, from, context)?;
        }
        let scanned = &self.scanned[&from];
        for &t in targets.iter() {
            if t != from && scanned.contains(&t) {
                out.insert(t);
            }
        }
        Ok(())
    }

    /// An edge from `source` was inserted: drop the walks that read its
    /// adjacency.
    pub(super) fn invalidate(&mut self, source: Vertex<ChangeId>) {
        if let Some(walks) = self.expanded_by.remove(&source) {
            for w in walks {
                if let Some(s) = self.scanned.remove(&w) {
                    self.stored -= s.len();
                }
            }
        }
    }

    /// `forward_reachable_within`'s walk, without targets.
    fn walk<T: GraphTxnT>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        from: Vertex<ChangeId>,
        context: &mut DeadContext,
    ) -> Result<(), BlockError<T::GraphError>> {
        let mut scanned = FxHashSet::default();
        let mut visited = std::mem::take(&mut context.scratch_visited);
        let mut stack = std::mem::take(&mut context.scratch_stack);
        visited.clear();
        stack.clear();
        let result: Result<(), BlockError<T::GraphError>> = (|| {
            visited.insert(from);
            stack.push(from);
            let mut budget = PROBE_BUDGET;
            while let Some(v) = stack.pop() {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                if v != from && !context.is_alive(txn, channel, v)? {
                    continue;
                }
                self.expanded_by.entry(v).or_default().push(from);
                for &c in context.forward_edges(txn, channel, v)? {
                    scanned.insert(c);
                    if visited.insert(c) {
                        stack.push(c)
                    }
                }
            }
            Ok(())
        })();
        context.scratch_visited = visited;
        context.scratch_stack = stack;
        result?;
        self.stored += scanned.len() + PROBE_BUDGET;
        self.scanned.insert(from, scanned);
        Ok(())
    }
}
