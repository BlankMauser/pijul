use super::*;

// Count pristine adjacency searches, not elapsed time: this regression must
// catch the quadratic algorithm independently of machine/load differences.
struct CountReads<'a, T> {
    inner: &'a T,
    reads: std::cell::Cell<usize>,
}

impl<T: GraphTxnT> GraphTxnT for CountReads<'_, T> {
    type Graph = T::Graph;
    type GraphError = T::GraphError;
    type Adj = T::Adj;

    fn get_graph<'a>(
        &'a self,
        g: &Self::Graph,
        k: &Vertex<ChangeId>,
        v: Option<&SerializedEdge>,
    ) -> Result<Option<&'a SerializedEdge>, TxnErr<Self::GraphError>> {
        self.inner.get_graph(g, k, v)
    }
    fn get_external(
        &self,
        p: &ChangeId,
    ) -> Result<&SerializedHash, GetExternalError<Self::GraphError>> {
        self.inner.get_external(p)
    }
    fn get_internal(
        &self,
        p: &SerializedHash,
    ) -> Result<Option<&ChangeId>, TxnErr<Self::GraphError>> {
        self.inner.get_internal(p)
    }
    fn init_adj(
        &self,
        g: &Self::Graph,
        v: Vertex<ChangeId>,
        d: Position<ChangeId>,
        min: EdgeFlags,
        max: EdgeFlags,
    ) -> Result<Self::Adj, TxnErr<Self::GraphError>> {
        self.reads.set(self.reads.get() + 1);
        self.inner.init_adj(g, v, d, min, max)
    }
    fn next_adj<'a>(
        &'a self,
        g: &Self::Graph,
        a: &mut Self::Adj,
    ) -> Option<Result<&'a SerializedEdge, TxnErr<Self::GraphError>>> {
        self.inner.next_adj(g, a)
    }
    fn find_block<'a>(
        &'a self,
        g: &Self::Graph,
        p: Position<ChangeId>,
    ) -> Result<&'a Vertex<ChangeId>, BlockError<Self::GraphError>> {
        self.inner.find_block(g, p)
    }
    fn find_block_end<'a>(
        &'a self,
        g: &Self::Graph,
        p: Position<ChangeId>,
    ) -> Result<&'a Vertex<ChangeId>, BlockError<Self::GraphError>> {
        self.inner.find_block_end(g, p)
    }
}

fn vertex(i: u64) -> Vertex<ChangeId> {
    Vertex {
        change: ChangeId(L64(42)),
        start: ChangePosition(L64(2 * i)),
        end: ChangePosition(L64(2 * i + 1)),
    }
}

#[test]
fn batched_justification_matches_single_target_search() -> Result<(), anyhow::Error> {
    let env = sanakirja::Pristine::new_anon()?;
    let mut txn = env.mut_txn_begin()?;
    let channel =
        txn.open_or_create_channel(&crate::small_string::SmallString::from_str("test"))?;
    let mut channel = channel.write();
    let graph = &mut *channel;
    let real = ChangeId(L64(42));
    let deleted = EdgeFlags::BLOCK | EdgeFlags::DELETED;
    // A dead cycle, alive exits, an alive barrier, a folder-only path, and
    // a pseudo-only path. Neither folders nor pseudos may justify a shortcut.
    for (a, b, flags) in [
        (Vertex::ROOT, vertex(1), deleted),
        (vertex(1), vertex(2), deleted),
        (vertex(2), vertex(1), deleted),
        (vertex(2), vertex(3), EdgeFlags::BLOCK),
        (vertex(3), vertex(4), EdgeFlags::BLOCK),
        (
            Vertex::ROOT,
            vertex(5),
            EdgeFlags::FOLDER | EdgeFlags::BLOCK,
        ),
    ] {
        put_graph_with_rev(&mut txn, graph, flags, a, b, real)?;
    }
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::PSEUDO,
        Vertex::ROOT,
        vertex(6),
        ChangeId::ROOT,
    )?;
    let all: HashSet<_> = (1..=6).map(vertex).collect();
    let mut expected = HashSet::default();
    let mut stack = Vec::new();
    let mut visited = HashSet::default();
    for &to in &all {
        if !pseudo_edge_justified(&txn, graph, Vertex::ROOT, to, &mut stack, &mut visited)? {
            expected.insert(to);
        }
    }
    let mut actual = all;
    retain_unjustified_pseudo_targets(
        &txn,
        graph,
        Vertex::ROOT,
        &mut actual,
        &mut stack,
        &mut visited,
        &mut DeadContext::new(false),
    )?;
    assert_eq!(actual, expected);
    assert_eq!(
        actual,
        [vertex(4), vertex(5), vertex(6)].into_iter().collect()
    );
    Ok(())
}

#[test]
fn cached_dead_frontier_tracks_new_pseudo_edges() -> Result<(), anyhow::Error> {
    let env = sanakirja::Pristine::new_anon()?;
    let mut txn = env.mut_txn_begin()?;
    let channel =
        txn.open_or_create_channel(&crate::small_string::SmallString::from_str("test"))?;
    let mut channel = channel.write();
    let graph = &mut *channel;
    let real = ChangeId(L64(42));
    let deleted = EdgeFlags::BLOCK | EdgeFlags::DELETED;
    for (a, b, flags) in [
        (Vertex::ROOT, vertex(1), deleted),
        (vertex(1), vertex(2), deleted),
        (vertex(2), vertex(1), deleted),
        (vertex(2), vertex(3), EdgeFlags::BLOCK),
        (vertex(3), vertex(4), EdgeFlags::BLOCK),
        (Vertex::ROOT, vertex(10), EdgeFlags::BLOCK),
    ] {
        put_graph_with_rev(&mut txn, graph, flags, a, b, real)?;
    }
    let mut context = DeadContext::new(true);
    let mut exits = HashSet::default();
    alive_exits_through_dead(&txn, graph, vertex(1), &mut exits, &mut context)?;
    assert_eq!(exits, [vertex(3)].into_iter().collect());
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::PSEUDO,
        vertex(2),
        vertex(10),
        ChangeId::ROOT,
    )?;
    context.invalidate(vertex(2));
    exits.clear();
    alive_exits_through_dead(&txn, graph, vertex(1), &mut exits, &mut context)?;
    assert_eq!(exits, [vertex(3), vertex(10)].into_iter().collect());
    exits.clear();
    alive_exits_through_dead(&txn, graph, vertex(3), &mut exits, &mut context)?;
    assert!(exits.is_empty());
    Ok(())
}

#[test]
fn clean_pseudo_fan_over_long_deleted_region() -> Result<(), anyhow::Error> {
    let env = sanakirja::Pristine::new_anon()?;
    let mut txn = env.mut_txn_begin()?;
    let channel =
        txn.open_or_create_channel(&crate::small_string::SmallString::from_str("test"))?;
    let mut channel = channel.write();
    let graph = &mut *channel;
    let real = ChangeId(L64(42));
    let n = 1024;
    let mut prev = Vertex::ROOT;
    for i in 1..=n {
        put_graph_with_rev(
            &mut txn,
            graph,
            EdgeFlags::BLOCK | EdgeFlags::DELETED,
            prev,
            vertex(i),
            real,
        )?;
        prev = vertex(i);
    }
    for i in n + 1..=2 * n {
        put_graph_with_rev(&mut txn, graph, EdgeFlags::BLOCK, prev, vertex(i), real)?;
        put_graph_with_rev(
            &mut txn,
            graph,
            EdgeFlags::PSEUDO,
            Vertex::ROOT,
            vertex(i),
            ChangeId::ROOT,
        )?;
    }
    let unjustified = vertex(2 * n + 1);
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::PSEUDO,
        Vertex::ROOT,
        unjustified,
        ChangeId::ROOT,
    )?;
    {
        let counted = CountReads {
            inner: &txn,
            reads: std::cell::Cell::new(0),
        };
        let mut stack = Vec::new();
        let mut visited = HashSet::default();
        // Even a sample of the old per-target searches exceeds the linear
        // budget below. This is the red side of the performance regression.
        for i in n + 1..=n + 64 {
            assert!(pseudo_edge_justified(
                &counted,
                graph,
                Vertex::ROOT,
                vertex(i),
                &mut stack,
                &mut visited
            )?);
        }
        let budget = 4 * n as usize + 8;
        assert!(counted.reads.get() > 8 * budget);
        counted.reads.set(0);
        let mut targets: HashSet<_> = (n + 1..=2 * n + 1).map(vertex).collect();
        retain_unjustified_pseudo_targets(
            &counted,
            graph,
            Vertex::ROOT,
            &mut targets,
            &mut stack,
            &mut visited,
            &mut DeadContext::new(false),
        )?;
        assert_eq!(targets, [unjustified].into_iter().collect());
        assert!(
            counted.reads.get() <= budget,
            "{} adjacency searches exceed {budget}",
            counted.reads.get()
        );
    }
    let inodes = [Vertex::ROOT.start_pos()].into_iter().collect();
    clean_unjustified_pseudo_edges(&mut txn, graph, &inodes)?;
    let remaining: HashSet<_> = iter_adjacent(
        &txn,
        graph,
        Vertex::ROOT,
        EdgeFlags::empty(),
        EdgeFlags::all(),
    )?
    .filter_map(|e| match e {
        Ok(e) if e.flag() == EdgeFlags::PSEUDO => Some(e.dest()),
        Ok(_) => None,
        Err(e) => panic!("{e:?}"),
    })
    .collect();
    assert_eq!(
        remaining,
        (n + 1..=2 * n).map(|i| vertex(i).start_pos()).collect()
    );
    Ok(())
}

#[test]
fn cached_probes_preserve_budget_order_and_invalidation() -> Result<(), anyhow::Error> {
    let env = sanakirja::Pristine::new_anon()?;
    let mut txn = env.mut_txn_begin()?;
    let channel =
        txn.open_or_create_channel(&crate::small_string::SmallString::from_str("test"))?;
    let mut channel = channel.write();
    let graph = &mut *channel;
    let real = ChangeId(L64(42));
    let n = PROBE_BUDGET as u64;
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::BLOCK,
        Vertex::ROOT,
        vertex(0),
        real,
    )?;
    for i in 0..=n {
        put_graph_with_rev(
            &mut txn,
            graph,
            EdgeFlags::BLOCK,
            vertex(i),
            vertex(i + 1),
            real,
        )?;
    }
    let mut context = DeadContext::new(true);
    let targets: HashSet<_> = [vertex(0), vertex(n), vertex(n + 1)].into_iter().collect();
    let mut reached = HashSet::default();
    {
        let counted = CountReads {
            inner: &txn,
            reads: std::cell::Cell::new(0),
        };
        // Warm exactly the 512 popped vertices, then repeat all three probes.
        assert!(!is_forward_reachable(
            &counted,
            graph,
            vertex(0),
            vertex(n + 1),
            &mut context
        )?);
        assert!(counted.reads.get() <= 2 * PROBE_BUDGET);
        counted.reads.set(0);
        for _ in 0..32 {
            assert!(is_forward_reachable(
                &counted,
                graph,
                vertex(0),
                vertex(n),
                &mut context
            )?);
            assert!(!is_forward_reachable(
                &counted,
                graph,
                vertex(0),
                vertex(n + 1),
                &mut context
            )?);
            assert!(reaches_any_within(
                &counted,
                graph,
                vertex(0),
                &targets,
                &mut context
            )?);
            forward_reachable_within(
                &counted,
                graph,
                vertex(0),
                &targets,
                &mut reached,
                &mut context,
            )?;
            assert_eq!(reached, [vertex(n)].into_iter().collect());
        }
        assert_eq!(
            counted.reads.get(),
            0,
            "repeated bounded probes must not rescan pristine"
        );
    }
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::PSEUDO,
        vertex(0),
        vertex(n + 1),
        ChangeId::ROOT,
    )?;
    context.invalidate(vertex(0));
    // The real edge precedes the pseudo edge in the pristine cursor. Stack
    // traversal therefore spends one pop on n+1 before walking the chain.
    assert_eq!(
        context.forward_edges(&txn, graph, vertex(0))?,
        &[vertex(1), vertex(n + 1)]
    );
    assert!(is_forward_reachable(
        &txn,
        graph,
        vertex(0),
        vertex(n + 1),
        &mut context
    )?);
    forward_reachable_within(&txn, graph, vertex(0), &targets, &mut reached, &mut context)?;
    assert_eq!(reached, [vertex(n + 1)].into_iter().collect());
    Ok(())
}

#[test]
fn oversized_frontier_matrix_falls_back_at_boundary() -> Result<(), anyhow::Error> {
    let env = sanakirja::Pristine::new_anon()?;
    let mut txn = env.mut_txn_begin()?;
    let channel =
        txn.open_or_create_channel(&crate::small_string::SmallString::from_str("test"))?;
    let mut channel = channel.write();
    let graph = &mut *channel;
    let real = ChangeId(L64(42));
    // 32,768 dead rows * 256 words * 8 bytes is exactly 64 MiB.
    let dead_count = 32_768;
    let alive_count = 16_384;
    let first_alive = vertex(dead_count + 1);
    for i in 1..=dead_count + alive_count {
        let flags = if i <= dead_count {
            EdgeFlags::BLOCK | EdgeFlags::DELETED
        } else {
            EdgeFlags::BLOCK
        };
        put_graph_with_rev(&mut txn, graph, flags, Vertex::ROOT, vertex(i), real)?;
    }
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::PSEUDO,
        vertex(1),
        first_alive,
        ChangeId::ROOT,
    )?;
    let mut vertices = (1..=dead_count + alive_count).map(vertex).collect();
    let snapshot = DeadExits::new(&txn, graph, &vertices)?.expect("matrix at cap fits");
    let mut exits = HashSet::default();
    snapshot.extend(vertex(1), &mut exits);
    assert_eq!(exits, [first_alive].into_iter().collect());
    drop(snapshot);

    // One extra dead row crosses the limit without changing the word count.
    let extra_dead = vertex(dead_count + alive_count + 1);
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::BLOCK | EdgeFlags::DELETED,
        Vertex::ROOT,
        extra_dead,
        real,
    )?;
    vertices.insert(extra_dead);
    let counted = CountReads {
        inner: &txn,
        reads: std::cell::Cell::new(0),
    };
    assert!(DeadExits::new(&counted, graph, &vertices)?.is_none());
    assert_eq!(
        counted.reads.get(),
        vertices.len(),
        "fallback precedes adjacency materialization"
    );
    vertices.remove(&extra_dead);

    // One extra alive exit rounds up to 257 words per row. The fallback
    // must keep this last exit, rather than truncate the frontier to fit.
    let extra_alive = vertex(dead_count + alive_count + 2);
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::BLOCK,
        Vertex::ROOT,
        extra_alive,
        real,
    )?;
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::PSEUDO,
        vertex(1),
        extra_alive,
        ChangeId::ROOT,
    )?;
    vertices.insert(extra_alive);
    let counted = CountReads {
        inner: &txn,
        reads: std::cell::Cell::new(0),
    };
    assert!(DeadExits::new(&counted, graph, &vertices)?.is_none());
    assert_eq!(
        counted.reads.get(),
        vertices.len(),
        "fallback precedes adjacency materialization"
    );
    exits.clear();
    alive_exits_through_dead(
        &txn,
        graph,
        vertex(1),
        &mut exits,
        &mut DeadContext::new(true),
    )?;
    assert_eq!(exits, [first_alive, extra_alive].into_iter().collect());
    Ok(())
}

#[test]
fn frontier_snapshot_matches_walks_and_invalidates_dead_sources() -> Result<(), anyhow::Error> {
    let env = sanakirja::Pristine::new_anon()?;
    let mut txn = env.mut_txn_begin()?;
    let channel =
        txn.open_or_create_channel(&crate::small_string::SmallString::from_str("test"))?;
    let mut channel = channel.write();
    let graph = &mut *channel;
    let real = ChangeId(L64(42));
    for (a, b, flags) in [
        (
            Vertex::ROOT,
            vertex(1),
            EdgeFlags::DELETED | EdgeFlags::BLOCK,
        ),
        (vertex(1), vertex(2), EdgeFlags::DELETED | EdgeFlags::BLOCK),
        (vertex(2), vertex(1), EdgeFlags::DELETED | EdgeFlags::BLOCK),
        (vertex(2), vertex(3), EdgeFlags::BLOCK),
        (vertex(3), vertex(4), EdgeFlags::BLOCK),
        (vertex(2), vertex(5), EdgeFlags::PSEUDO),
        (vertex(5), vertex(6), EdgeFlags::BLOCK),
        (Vertex::ROOT, vertex(7), EdgeFlags::BLOCK),
    ] {
        let introduced = if flags.contains(EdgeFlags::PSEUDO) {
            ChangeId::ROOT
        } else {
            real
        };
        put_graph_with_rev(&mut txn, graph, flags, a, b, introduced)?;
    }
    let vertices = std::iter::once(Vertex::ROOT)
        .chain((1..=7).map(vertex))
        .collect();
    let mut snapshot = DeadExits::new(&txn, graph, &vertices)?;
    let mut context = DeadContext::new(true);
    for &v in &vertices {
        let mut cached = HashSet::default();
        snapshot.as_ref().unwrap().extend(v, &mut cached);
        let mut walked = HashSet::default();
        alive_exits_through_dead(&txn, graph, v, &mut walked, &mut context)?;
        assert_eq!(cached, walked, "frontier of {v:?}");
    }
    let mut cached = HashSet::default();
    snapshot.as_ref().unwrap().extend(vertex(1), &mut cached);
    assert_eq!(cached, [vertex(3), vertex(6)].into_iter().collect());
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::PSEUDO,
        vertex(3),
        vertex(7),
        ChangeId::ROOT,
    )?;
    context.invalidate(vertex(3));
    DeadExits::invalidate_after_insert(&mut snapshot, vertex(3), vertex(7));
    assert!(snapshot.is_some());
    let mut walked = HashSet::default();
    alive_exits_through_dead(&txn, graph, vertex(1), &mut walked, &mut context)?;
    assert_eq!(cached, walked);
    for to in [vertex(6), vertex(2)] {
        put_graph_with_rev(
            &mut txn,
            graph,
            EdgeFlags::PSEUDO,
            vertex(1),
            to,
            ChangeId::ROOT,
        )?;
        context.invalidate(vertex(1));
        DeadExits::invalidate_after_insert(&mut snapshot, vertex(1), to);
        assert!(
            snapshot.is_some(),
            "redundant frontier edge discarded snapshot"
        );
        walked.clear();
        alive_exits_through_dead(&txn, graph, vertex(1), &mut walked, &mut context)?;
        assert_eq!(cached, walked);
    }
    put_graph_with_rev(
        &mut txn,
        graph,
        EdgeFlags::PSEUDO,
        vertex(1),
        vertex(7),
        ChangeId::ROOT,
    )?;
    context.invalidate(vertex(1));
    DeadExits::invalidate_after_insert(&mut snapshot, vertex(1), vertex(7));
    assert!(snapshot.is_none());
    walked.clear();
    alive_exits_through_dead(&txn, graph, vertex(1), &mut walked, &mut context)?;
    assert_eq!(
        walked,
        [vertex(3), vertex(6), vertex(7)].into_iter().collect()
    );
    let incomplete = [Vertex::ROOT, vertex(1)].into_iter().collect();
    assert!(DeadExits::new(&txn, graph, &incomplete)?.is_none());
    let mut snapshot = DeadExits::new(&txn, graph, &vertices)?;
    DeadExits::invalidate_after_insert(&mut snapshot, vertex(999), vertex(1));
    assert!(snapshot.is_none());
    let mut snapshot = DeadExits::new(&txn, graph, &vertices)?;
    DeadExits::invalidate_after_insert(&mut snapshot, vertex(1), vertex(999));
    assert!(snapshot.is_none());
    Ok(())
}

/// Tiny deterministic generator, so the randomized graphs below are the same
/// on every run.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

/// A random graph of `n` vertices below the root: a random spanning tree of
/// alive and deleted edges, then `extra` random edges (cycles included) that
/// are alive, deleted or pseudo.
fn random_graph<T: GraphMutTxnT>(
    txn: &mut T,
    graph: &mut T::Graph,
    rng: &mut Lcg,
    n: u64,
    extra: u64,
) -> Result<(), anyhow::Error> {
    let real = ChangeId(L64(42));
    let deleted = EdgeFlags::BLOCK | EdgeFlags::DELETED;
    let pick = |rng: &mut Lcg| match rng.next(3) {
        0 => EdgeFlags::BLOCK,
        1 => deleted,
        _ => EdgeFlags::PSEUDO,
    };
    for i in 1..=n {
        let p = rng.next(i);
        let from = if p == 0 { Vertex::ROOT } else { vertex(p) };
        let flags = if rng.next(2) == 0 {
            EdgeFlags::BLOCK
        } else {
            deleted
        };
        put_graph_with_rev(txn, graph, flags, from, vertex(i), real)?;
    }
    for _ in 0..extra {
        let (a, b) = (1 + rng.next(n), 1 + rng.next(n));
        if a == b {
            continue;
        }
        let flags = pick(rng);
        let intro = if flags.contains(EdgeFlags::PSEUDO) {
            ChangeId::ROOT
        } else {
            real
        };
        put_graph_with_rev(txn, graph, flags, vertex(a), vertex(b), intro)?;
    }
    Ok(())
}

#[test]
fn exits_memo_matches_walks_on_random_graphs() -> Result<(), anyhow::Error> {
    for seed in 0..40u64 {
        let env = sanakirja::Pristine::new_anon()?;
        let mut txn = env.mut_txn_begin()?;
        let channel =
            txn.open_or_create_channel(&crate::small_string::SmallString::from_str("test"))?;
        let mut channel = channel.write();
        let graph = &mut *channel;
        let mut rng = Lcg(seed);
        let n = 30 + rng.next(60);
        random_graph(&mut txn, graph, &mut rng, n, 2 * n)?;
        let mut memo = ExitsMemo::default();
        let mut context = DeadContext::new(true);
        for round in 0..4 {
            for i in (0..=n).rev() {
                let v = if i == 0 { Vertex::ROOT } else { vertex(i) };
                let mut walked = HashSet::default();
                alive_exits_through_dead(&txn, graph, v, &mut walked, &mut DeadContext::new(true))?;
                let mut cached = FxHashSet::default();
                memo.extend(&txn, graph, v, &mut cached, &mut context)?;
                assert_eq!(
                    cached
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>(),
                    walked.iter().copied().collect(),
                    "seed {seed} round {round} vertex {v:?}"
                );
            }
            // Insert pseudo-edges as the repair does, with its invalidation.
            for _ in 0..3 {
                let (a, b) = (1 + rng.next(n), 1 + rng.next(n));
                if a == b {
                    continue;
                }
                if put_graph_with_rev(
                    &mut txn,
                    graph,
                    EdgeFlags::PSEUDO,
                    vertex(a),
                    vertex(b),
                    ChangeId::ROOT,
                )? {
                    if !context.is_alive(&txn, graph, vertex(a))? {
                        memo.clear();
                    }
                    context.invalidate(vertex(a));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn scan_memo_matches_bounded_probes() -> Result<(), anyhow::Error> {
    for seed in 0..12u64 {
        let env = sanakirja::Pristine::new_anon()?;
        let mut txn = env.mut_txn_begin()?;
        let channel =
            txn.open_or_create_channel(&crate::small_string::SmallString::from_str("test"))?;
        let mut channel = channel.write();
        let graph = &mut *channel;
        let mut rng = Lcg(1000 + seed);
        // Large enough for the probes to run out of budget.
        let n = 2 * PROBE_BUDGET as u64 + rng.next(200);
        random_graph(&mut txn, graph, &mut rng, n, n / 2)?;
        let mut memo = ScanMemo::default();
        let mut context = DeadContext::new(true);
        for round in 0..6 {
            for _ in 0..60 {
                let from = vertex(1 + rng.next(n));
                let mut targets: HashSet<_> = HashSet::default();
                for _ in 0..1 + rng.next(8) {
                    targets.insert(vertex(1 + rng.next(n)));
                }
                if rng.next(4) == 0 {
                    targets.insert(from);
                }
                let mut direct = HashSet::default();
                forward_reachable_within(
                    &txn,
                    graph,
                    from,
                    &targets,
                    &mut direct,
                    &mut DeadContext::new(true),
                )?;
                let mut cached = HashSet::default();
                memo.reachable_within(&txn, graph, from, &targets, &mut cached, &mut context)?;
                assert_eq!(cached, direct, "seed {seed} round {round} from {from:?}");
            }
            for _ in 0..5 {
                let (a, b) = (1 + rng.next(n), 1 + rng.next(n));
                if a == b {
                    continue;
                }
                if put_graph_with_rev(
                    &mut txn,
                    graph,
                    EdgeFlags::PSEUDO,
                    vertex(a),
                    vertex(b),
                    ChangeId::ROOT,
                )? {
                    context.invalidate(vertex(a));
                    memo.invalidate(vertex(a));
                }
            }
            // An edge from a walk's own source changes that walk's answer.
            for _ in 0..5 {
                let (a, b) = (vertex(1 + rng.next(n)), vertex(1 + rng.next(n)));
                if a == b {
                    continue;
                }
                let targets: HashSet<_> = [b].into_iter().collect();
                let mut before = HashSet::default();
                memo.reachable_within(&txn, graph, a, &targets, &mut before, &mut context)?;
                if put_graph_with_rev(&mut txn, graph, EdgeFlags::PSEUDO, a, b, ChangeId::ROOT)? {
                    context.invalidate(a);
                    memo.invalidate(a);
                }
                let mut after = HashSet::default();
                memo.reachable_within(&txn, graph, a, &targets, &mut after, &mut context)?;
                assert_eq!(after, targets, "seed {seed} new edge {a:?} -> {b:?}");
            }
        }
    }
    Ok(())
}

#[test]
fn direct_child_shortcut_answers_like_the_search() -> Result<(), anyhow::Error> {
    for seed in 0..20u64 {
        let env = sanakirja::Pristine::new_anon()?;
        let mut txn = env.mut_txn_begin()?;
        let channel =
            txn.open_or_create_channel(&crate::small_string::SmallString::from_str("test"))?;
        let mut channel = channel.write();
        let graph = &mut *channel;
        let mut rng = Lcg(5000 + seed);
        let n = 40 + rng.next(80);
        random_graph(&mut txn, graph, &mut rng, n, 3 * n)?;
        let mut context = DeadContext::new(true);
        for a in 1..=n {
            for b in 1..=n {
                let (a, b) = (vertex(a), vertex(b));
                // The search this shortcut stands for, written out in full.
                let expected = a == b || {
                    let mut visited = HashSet::default();
                    visited.insert(a);
                    let mut stack = vec![a];
                    let mut budget = PROBE_BUDGET;
                    let mut found = false;
                    'search: while let Some(w) = stack.pop() {
                        if budget == 0 {
                            break;
                        }
                        budget -= 1;
                        if w != a && !is_alive(&txn, graph, &w)? {
                            continue;
                        }
                        let mut children = Vec::new();
                        for e in iter_adjacent(
                            &txn,
                            graph,
                            w,
                            EdgeFlags::empty(),
                            EdgeFlags::all() - EdgeFlags::DELETED - EdgeFlags::PARENT,
                        )? {
                            children.push(*txn.find_block(graph, e?.dest())?);
                        }
                        for c in children {
                            if c == b {
                                found = true;
                                break 'search;
                            }
                            if visited.insert(c) {
                                stack.push(c)
                            }
                        }
                    }
                    found
                };
                assert_eq!(
                    is_forward_reachable(&txn, graph, a, b, &mut context)?,
                    expected,
                    "seed {seed} {a:?} -> {b:?}"
                );
            }
        }
    }
    Ok(())
}
