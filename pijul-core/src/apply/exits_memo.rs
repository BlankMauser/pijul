//! Alive exits of dead vertices, computed once per zombie repair and shared.
//!
//! When the dense `DeadExits` snapshot does not fit (or was invalidated), the
//! repair used to walk the dead region below every revisited dead vertex with
//! a fresh visited set: a long deleted region is then walked once per vertex in
//! it. The exits of a dead vertex are the alive vertices it reaches through
//! dead vertices only, so they are the union of its alive children and of its
//! dead children's exits; inside a cycle of dead vertices they are all the
//! same. One Tarjan pass over the dead region computes them per strongly
//! connected component, and every later query reads the stored answer.
//!
//! The answers read the same `DeadContext` adjacency and aliveness as the walk
//! (`alive_exits_through_dead`), so they are the walk's answers exactly. A
//! pseudo-edge from an alive source changes no answer, since no walk expands an
//! alive vertex; the caller clears the memo whenever it inserts an edge from a
//! dead source.
use super::dead_context::DeadContext;
use crate::pristine::*;
use rustc_hash::FxHashMap;
use std::collections::HashSet;
use std::hash::BuildHasher;
use std::rc::Rc;

type Exits = Rc<[Vertex<ChangeId>]>;

/// Budget, in vertices, for the stored answers plus the union being built.
/// The budget is checked before each union is allocated, so the retained and
/// accounted vertex payload never exceeds this (about 192 MiB by default,
/// before overhead). It is not a total-memory cap: the traversal buffers
/// (`index`, `vertices`, `low`, `scc_stack`, `calls`), the adjacency
/// copies (`children` per frame, `alive`, `below`) and the direct-child
/// collection (`direct`, `parts`) are allocated before the check and are
/// not counted. `peak()` reports this accounting, not actual allocation.
/// A computation that would exceed it stops, and
/// the query falls back to the exact walk instead: dropping or never storing
/// answers only costs recomputing them, never reachability.
const MAX_STORED: usize = 1 << 23;

pub(super) struct ExitsMemo {
    done: FxHashMap<Vertex<ChangeId>, Exits>,
    stored: usize,
    budget: usize,
    /// Highest `stored` plus union in flight seen, for the tests.
    peak: usize,
    /// A computation from an empty memo did not fit: answer uncached queries
    /// with the walk until the next `clear`.
    full: bool,
}

impl Default for ExitsMemo {
    fn default() -> Self {
        Self::with_budget(MAX_STORED)
    }
}

struct Frame {
    idx: usize,
    children: Vec<(Vertex<ChangeId>, bool)>,
    pos: usize,
}

impl ExitsMemo {
    pub(super) fn with_budget(budget: usize) -> Self {
        ExitsMemo {
            done: FxHashMap::default(),
            stored: 0,
            budget,
            peak: 0,
            full: false,
        }
    }

    /// Vertices held by the distinct stored answers, counted independently
    /// of the memo's own accounting.
    #[cfg(test)]
    pub(super) fn stored_elements(&self) -> usize {
        let mut seen = HashSet::new();
        self.done
            .values()
            .filter(|e| seen.insert(Rc::as_ptr(e) as *const Vertex<ChangeId>))
            .map(|e| e.len())
            .sum()
    }

    #[cfg(test)]
    pub(super) fn peak(&self) -> usize {
        self.peak
    }

    pub(super) fn clear(&mut self) {
        self.done.clear();
        self.stored = 0;
        self.full = false;
    }

    /// Adds to `out` the first alive vertices below the dead vertex `d`, going
    /// through dead vertices only. Same answer as `alive_exits_through_dead`.
    pub(super) fn extend<T: GraphTxnT, S: BuildHasher>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        d: Vertex<ChangeId>,
        out: &mut HashSet<Vertex<ChangeId>, S>,
        context: &mut DeadContext,
    ) -> Result<(), BlockError<T::GraphError>> {
        if context.is_alive(txn, channel, d)? {
            return Ok(());
        }
        if !self.done.contains_key(&d) {
            let mut fits = !self.full && self.compute(txn, channel, d, context)?;
            if !fits && !self.full && self.stored > 0 {
                // Earlier answers took the room: start over once.
                self.clear();
                fits = self.compute(txn, channel, d, context)?;
            }
            if !fits {
                // Even alone, the answers below `d` do not fit: walk.
                self.full = true;
                return super::alive_exits_through_dead(txn, channel, d, out, context);
            }
        }
        out.extend(self.done[&d].iter().copied());
        Ok(())
    }

    /// Iterative Tarjan over the dead vertices reachable from the dead `d`
    /// through dead vertices, skipping components already answered. Returns
    /// `false`, with `d` unanswered, when storing the answers would exceed the
    /// budget; the components completed before that keep their (exact)
    /// answers.
    fn compute<T: GraphTxnT>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        d: Vertex<ChangeId>,
        context: &mut DeadContext,
    ) -> Result<bool, BlockError<T::GraphError>> {
        let mut index: FxHashMap<Vertex<ChangeId>, usize> = FxHashMap::default();
        let mut vertices = Vec::new();
        let mut low = Vec::new();
        let mut on_stack = Vec::new();
        // Per vertex: alive children, and answers of completed child components.
        let mut alive = Vec::new();
        let mut below: Vec<Vec<Exits>> = Vec::new();
        let mut scc_stack = Vec::new();
        let mut calls: Vec<Frame> = Vec::new();

        let open = |v: Vertex<ChangeId>,
                    context: &mut DeadContext,
                    index: &mut FxHashMap<Vertex<ChangeId>, usize>,
                    vertices: &mut Vec<Vertex<ChangeId>>,
                    low: &mut Vec<usize>,
                    on_stack: &mut Vec<bool>,
                    alive: &mut Vec<Vec<Vertex<ChangeId>>>,
                    below: &mut Vec<Vec<Exits>>,
                    scc_stack: &mut Vec<usize>|
         -> Result<Frame, BlockError<T::GraphError>> {
            let idx = vertices.len();
            index.insert(v, idx);
            vertices.push(v);
            low.push(idx);
            on_stack.push(true);
            alive.push(Vec::new());
            below.push(Vec::new());
            scc_stack.push(idx);
            Ok(Frame {
                idx,
                children: context.edges(txn, channel, v)?.to_vec(),
                pos: 0,
            })
        };

        let f = open(
            d,
            context,
            &mut index,
            &mut vertices,
            &mut low,
            &mut on_stack,
            &mut alive,
            &mut below,
            &mut scc_stack,
        )?;
        calls.push(f);
        while let Some(frame) = calls.last_mut() {
            let idx = frame.idx;
            if frame.pos < frame.children.len() {
                let (c, c_alive) = frame.children[frame.pos];
                frame.pos += 1;
                if c_alive {
                    alive[idx].push(c);
                } else if let Some(e) = self.done.get(&c) {
                    below[idx].push(e.clone());
                } else if let Some(&j) = index.get(&c) {
                    // Not answered yet, hence still on the component stack.
                    debug_assert!(on_stack[j]);
                    low[idx] = low[idx].min(j);
                } else {
                    let f = open(
                        c,
                        context,
                        &mut index,
                        &mut vertices,
                        &mut low,
                        &mut on_stack,
                        &mut alive,
                        &mut below,
                        &mut scc_stack,
                    )?;
                    calls.push(f);
                }
                continue;
            }
            calls.pop();
            if low[idx] == idx {
                let start = scc_stack.iter().rposition(|&j| j == idx).unwrap();
                let members: Vec<usize> = scc_stack.drain(start..).collect();
                let mut direct = Vec::new();
                let mut parts: Vec<Exits> = Vec::new();
                for &m in &members {
                    on_stack[m] = false;
                    direct.append(&mut alive[m]);
                    parts.append(&mut below[m]);
                }
                parts.sort_unstable_by_key(|p| Rc::as_ptr(p) as *const Vertex<ChangeId>);
                parts.dedup_by(|a, b| Rc::ptr_eq(a, b));
                let exits: Exits = if direct.is_empty() && parts.len() == 1 {
                    parts.pop().unwrap()
                } else {
                    // The union is held twice at most (vector, then its
                    // shared copy): check it fits before building it.
                    let need = direct.len() + parts.iter().map(|p| p.len()).sum::<usize>();
                    let in_flight = self.stored.saturating_add(need.saturating_mul(2));
                    if in_flight > self.budget {
                        return Ok(false);
                    }
                    self.peak = self.peak.max(in_flight);
                    direct.reserve_exact(need - direct.len());
                    for p in &parts {
                        direct.extend(p.iter().copied());
                    }
                    direct.sort_unstable();
                    direct.dedup();
                    self.stored += direct.len();
                    direct.into()
                };
                for &m in &members {
                    self.done.insert(vertices[m], exits.clone());
                }
                if let Some(parent) = calls.last() {
                    below[parent.idx].push(exits);
                }
            } else if let Some(parent) = calls.last() {
                let p = parent.idx;
                low[p] = low[p].min(low[idx]);
            }
        }
        Ok(true)
    }
}
