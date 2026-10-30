//! Exact dead-vertex/alive-descendant relation, with 64 pairs per stored word.
//!
//! Long deleted regions below many alive exits make a BTreeSet of vertex pairs
//! quadratic in *allocated* memory (millions of 48-byte keys). Sparse words
//! retain every pair without allocating a dense dead-by-alive matrix. Queries
//! restore Vertex order, matching the old BTreeSet range iteration exactly.
use crate::pristine::{ChangeId, Vertex};
use rustc_hash::FxHashMap as HashMap;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Descendants {
    ids: HashMap<Vertex<ChangeId>, usize>,
    vertices: Vec<Vertex<ChangeId>>,
    words: BTreeMap<(Vertex<ChangeId>, usize), u64>,
}

impl Descendants {
    pub(super) fn insert(&mut self, (dead, alive): (Vertex<ChangeId>, Vertex<ChangeId>)) {
        let id = *self.ids.entry(alive).or_insert_with(|| {
            self.vertices.push(alive);
            self.vertices.len() - 1
        });
        *self.words.entry((dead, id / 64)).or_default() |= 1 << (id % 64);
    }

    pub(super) fn get(&self, dead: Vertex<ChangeId>) -> Vec<Vertex<ChangeId>> {
        let mut result = Vec::new();
        for ((_, word), bits) in self.words.range((dead, 0)..=(dead, usize::MAX)) {
            let mut bits = *bits;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                result.push(self.vertices[word * 64 + bit]);
                bits &= bits - 1;
            }
        }
        result.sort_unstable();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::{ChangePosition, L64};
    use std::collections::BTreeSet;

    fn vertex(i: u64) -> Vertex<ChangeId> {
        Vertex {
            change: ChangeId(L64(1)),
            start: ChangePosition(L64(2 * i)),
            end: ChangePosition(L64(2 * i + 1)),
        }
    }

    #[test]
    fn dense_relation_is_exact_and_compact() {
        let mut compact = Descendants::default();
        let mut original = BTreeSet::new();
        for dead in 0..200 {
            // Deliberately assign IDs in the reverse of Vertex order.
            for alive in (200..400).rev() {
                compact.insert((vertex(dead), vertex(alive)));
                compact.insert((vertex(dead), vertex(alive)));
                original.insert((vertex(dead), vertex(alive)));
            }
        }
        assert_eq!(original.len(), 40_000);
        assert_eq!(compact.words.len(), 200 * 4);
        for dead in 0..201 {
            let expected: Vec<_> = original
                .range((vertex(dead), Vertex::ROOT)..=(vertex(dead), Vertex::MAX))
                .map(|(_, v)| *v)
                .collect();
            assert_eq!(compact.get(vertex(dead)), expected);
        }
    }

    #[test]
    fn sparse_late_ids_do_not_allocate_a_dense_matrix() {
        let mut compact = Descendants::default();
        for i in 0..10_000 {
            compact.insert((vertex(i), vertex(i + 10_000)));
        }
        assert_eq!(compact.words.len(), 10_000);
        for i in 0..10_000 {
            assert_eq!(compact.get(vertex(i)), vec![vertex(i + 10_000)]);
        }
    }
}
