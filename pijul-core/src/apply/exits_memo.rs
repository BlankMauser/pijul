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

/// Stored answers beyond which the memo starts over. Dropping answers only
/// costs recomputing them.
const MAX_STORED: usize = 1 << 23;

#[derive(Default)]
pub(super) struct ExitsMemo {
    done: FxHashMap<Vertex<ChangeId>, Exits>,
    stored: usize,
}

struct Frame {
    idx: usize,
    children: Vec<(Vertex<ChangeId>, bool)>,
    pos: usize,
}

impl ExitsMemo {
    pub(super) fn clear(&mut self) {
        self.done.clear();
        self.stored = 0;
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
            if self.stored > MAX_STORED {
                self.clear();
            }
            self.compute(txn, channel, d, context)?;
        }
        out.extend(self.done[&d].iter().copied());
        Ok(())
    }

    /// Iterative Tarjan over the dead vertices reachable from the dead `d`
    /// through dead vertices, skipping components already answered.
    fn compute<T: GraphTxnT>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        d: Vertex<ChangeId>,
        context: &mut DeadContext,
    ) -> Result<(), BlockError<T::GraphError>> {
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
        Ok(())
    }
}
