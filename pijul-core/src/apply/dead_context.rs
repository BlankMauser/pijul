//! Read-through adjacency and aliveness caches for repair and cleanup.
//!
//! Real-edge changes and vertex splits precede these caches. Cleanup defers
//! deletions until after all searches. Zombie repair invalidates the source
//! adjacency whenever it inserts a pseudo-edge. Pseudo-edges never change real
//! aliveness, so the aliveness cache remains valid in both phases.
use crate::HashMap;
use crate::pristine::*;

type Neighbour = (Vertex<ChangeId>, bool);
type Neighbours = Vec<Neighbour>;

pub(super) struct DeadContext {
    include_pseudo: bool,
    alive: HashMap<Vertex<ChangeId>, bool>,
    edges: HashMap<Vertex<ChangeId>, Neighbours>,
    forward: HashMap<Vertex<ChangeId>, Vec<Vertex<ChangeId>>>,
}

impl DeadContext {
    pub(super) fn new(include_pseudo: bool) -> Self {
        Self {
            include_pseudo,
            alive: HashMap::default(),
            edges: HashMap::default(),
            forward: HashMap::default(),
        }
    }

    pub(super) fn invalidate(&mut self, v: Vertex<ChangeId>) {
        self.edges.remove(&v);
        self.forward.remove(&v);
    }

    pub(super) fn is_alive<T: GraphTxnT>(
        &mut self,
        txn: &T,
        graph: &T::Graph,
        v: Vertex<ChangeId>,
    ) -> Result<bool, BlockError<T::GraphError>> {
        if let Some(&alive) = self.alive.get(&v) {
            return Ok(alive);
        }
        let alive = is_alive(txn, graph, &v)?;
        self.alive.insert(v, alive);
        Ok(alive)
    }

    /// Keep the original cursor order and duplicates: the bounded probes'
    /// answers depend on the exact order in which their budget is spent.
    pub(super) fn forward_edges<T: GraphTxnT>(
        &mut self,
        txn: &T,
        graph: &T::Graph,
        v: Vertex<ChangeId>,
    ) -> Result<&[Vertex<ChangeId>], BlockError<T::GraphError>> {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.forward.entry(v) {
            let mut edges = Vec::new();
            for e in iter_adjacent(
                txn,
                graph,
                v,
                EdgeFlags::empty(),
                EdgeFlags::all() - EdgeFlags::DELETED - EdgeFlags::PARENT,
            )? {
                edges.push(*txn.find_block(graph, e?.dest())?);
            }
            entry.insert(edges);
        }
        Ok(self.forward.get(&v).unwrap())
    }

    pub(super) fn edges<T: GraphTxnT>(
        &mut self,
        txn: &T,
        graph: &T::Graph,
        v: Vertex<ChangeId>,
    ) -> Result<&[Neighbour], BlockError<T::GraphError>> {
        if !self.edges.contains_key(&v) {
            let mut edges = Vec::new();
            let max = if self.include_pseudo {
                EdgeFlags::all() - EdgeFlags::PARENT - EdgeFlags::FOLDER
            } else {
                EdgeFlags::all() - EdgeFlags::PARENT
            };
            for e in iter_adjacent(txn, graph, v, EdgeFlags::empty(), max)? {
                let e = e?;
                if e.flag().contains(EdgeFlags::PARENT)
                    || (!self.include_pseudo
                        && e.flag().intersects(EdgeFlags::FOLDER | EdgeFlags::PSEUDO))
                {
                    continue;
                }
                let c = *txn.find_block(graph, e.dest())?;
                let alive = self.is_alive(txn, graph, c)?;
                edges.push((c, alive));
            }
            self.edges.insert(v, edges);
        }
        Ok(self.edges.get(&v).unwrap())
    }
}
