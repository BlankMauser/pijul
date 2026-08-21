use crate::apply;
use crate::change::*;
use crate::changestore::*;
use crate::pristine::*;
use crate::working_copy::WorkingCopy;
use std::collections::{HashMap, HashSet};

mod working_copy;

#[derive(Error)]
pub enum UnrecordError<ChangestoreError: std::error::Error + 'static, T: GraphTxnT + TreeTxnT> {
    #[error("Changestore error: {0}")]
    Changestore(ChangestoreError),
    #[error(transparent)]
    Txn(#[from] TxnErr<T::GraphError>),
    #[error(transparent)]
    Tree(#[from] TreeErr<T::TreeError>),
    #[error(transparent)]
    Block(#[from] crate::pristine::BlockError<T::GraphError>),
    #[error(transparent)]
    InconsistentChange(#[from] crate::pristine::InconsistentChange<T::GraphError>),
    #[error("Change not in channel: {}", hash.to_base32())]
    ChangeNotInChannel { hash: ChangeId },
    #[error("Change {} is depended upon by {}", change_id.to_base32(), dependent.to_base32())]
    ChangeIsDependedUpon {
        change_id: ChangeId,
        dependent: ChangeId,
    },
    #[error(transparent)]
    Missing(#[from] crate::missing_context::MissingError<T::GraphError>),
    #[error(transparent)]
    LocalApply(#[from] crate::apply::LocalApplyError<T>),
    #[error(transparent)]
    Apply(#[from] crate::apply::ApplyError<ChangestoreError, T>),
    #[error(transparent)]
    SmallStr(#[from] crate::small_string::Error),
}

impl<C: std::error::Error, T: GraphTxnT + TreeTxnT> std::fmt::Debug for UnrecordError<C, T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            UnrecordError::Changestore(e) => std::fmt::Debug::fmt(e, fmt),
            UnrecordError::Txn(e) => std::fmt::Debug::fmt(e, fmt),
            UnrecordError::Tree(e) => std::fmt::Debug::fmt(e, fmt),
            UnrecordError::Block(e) => std::fmt::Debug::fmt(e, fmt),
            UnrecordError::InconsistentChange(e) => std::fmt::Debug::fmt(e, fmt),
            UnrecordError::ChangeNotInChannel { hash } => {
                write!(fmt, "Change not in channel: {}", hash.to_base32())
            }
            UnrecordError::ChangeIsDependedUpon {
                change_id,
                dependent,
            } => write!(
                fmt,
                "Change {} is depended upon: {}",
                change_id.to_base32(),
                dependent.to_base32()
            ),
            UnrecordError::Missing(e) => std::fmt::Debug::fmt(e, fmt),
            UnrecordError::LocalApply(e) => std::fmt::Debug::fmt(e, fmt),
            UnrecordError::Apply(e) => std::fmt::Debug::fmt(e, fmt),
            UnrecordError::SmallStr(e) => std::fmt::Debug::fmt(e, fmt),
        }
    }
}

pub type TouchedInodes = HashSet<(ChangeId, Position<Option<Hash>>)>;

/// Unrecord `hash` with "undo" intent: besides removing the change, this clears
/// obsolescence markers — it resurrects any predecessor `hash` had superseded
/// and drops `hash`'s own marker. Use [`unrecord_superseding`] instead when the
/// removal is part of a supersede (amend / `unrecord_superseded`), where markers
/// must be preserved so a chain of amends keeps its predecessors filtered.
pub fn unrecord<T: MutTxnT, P: ChangeStore>(
    txn: &mut T,
    channel: &ChannelRef<T>,
    changes: &P,
    hash: &Hash,
    salt: u64,
    touched: &mut TouchedInodes,
) -> Result<bool, UnrecordError<P::Error, T>> {
    unrecord_(txn, channel, changes, hash, salt, touched, false)
}

/// Like [`unrecord`] but for a supersede: leaves obsolescence markers untouched.
pub fn unrecord_superseding<T: MutTxnT, P: ChangeStore>(
    txn: &mut T,
    channel: &ChannelRef<T>,
    changes: &P,
    hash: &Hash,
    salt: u64,
    touched: &mut TouchedInodes,
) -> Result<bool, UnrecordError<P::Error, T>> {
    unrecord_(txn, channel, changes, hash, salt, touched, true)
}

fn unrecord_<T: MutTxnT, P: ChangeStore>(
    txn: &mut T,
    channel: &ChannelRef<T>,
    changes: &P,
    hash: &Hash,
    salt: u64,
    touched: &mut TouchedInodes,
    superseding: bool,
) -> Result<bool, UnrecordError<P::Error, T>> {
    let change_id = if let Some(&h) = txn.get_internal(&hash.into())? {
        h
    } else {
        return Ok(false);
    };
    let unused = unused_in_other_channels(txn, channel, change_id)?;
    let mut channel = channel.write();

    del_channel_changes::<T, P>(txn, &mut channel, change_id)?;

    let change = changes
        .get_change(hash)
        .map_err(UnrecordError::Changestore)?;

    unapply(
        txn,
        &mut channel,
        changes,
        change_id,
        &change,
        salt,
        touched,
    )?;

    if !superseding {
        // Undo intent: resurrect any predecessor this change superseded (drop the
        // marker keyed by it) and drop this change's own marker, so the table
        // doesn't accumulate and a re-pull of the predecessor is no longer skipped.
        if let Some(parent) = change.replaces() {
            txn.unmark_superseded(&parent)?;
        }
        txn.unmark_superseded(hash)?;
    }

    if unused {
        assert!(txn.get_revdep(&change_id, None)?.is_none());
        while txn.del_dep(&change_id, None)? {}
        txn.del_external(&change_id, None)?;
        txn.del_internal(&hash.into(), None)?;
        for dep in change.dependencies.iter() {
            let dep = *txn.get_internal(&dep.into())?.unwrap();
            txn.del_revdep(&dep, Some(&change_id))?;
        }
        Ok(false)
    } else {
        Ok(true)
    }
}

fn del_channel_changes<
    T: ChannelMutTxnT + DepsTxnT<DepsError = <T as GraphTxnT>::GraphError> + TreeTxnT,
    P: ChangeStore,
>(
    txn: &mut T,
    channel: &mut T::Channel,
    change_id: ChangeId,
) -> Result<(), UnrecordError<P::Error, T>> {
    let timestamp = if let Some(&ts) = txn.get_changeset(txn.changes(channel), &change_id)? {
        ts
    } else {
        return Err(UnrecordError::ChangeNotInChannel { hash: change_id });
    };
    debug!("del_channel_changes {:?}", change_id);
    for x in txn.iter_revdep(&change_id)? {
        debug!("revdep {:?}", x);
        let (p, d) = x?;
        assert!(*p >= change_id);
        if *p > change_id {
            break;
        }
        if txn.get_changeset(txn.changes(channel), d)?.is_some() {
            return Err(UnrecordError::ChangeIsDependedUpon {
                change_id,
                dependent: *d,
            });
        }
    }

    txn.del_changes(channel, change_id, timestamp.into())?;

    let tags = txn.tags_mut(channel);
    txn.del_tags(tags, timestamp.into())?;

    Ok(())
}

fn unused_in_other_channels<T: TxnT>(
    txn: &mut T,
    channel: &ChannelRef<T>,
    change_id: ChangeId,
) -> Result<bool, TxnErr<T::GraphError>> {
    let channel = channel.read();
    for br in txn.channels(&crate::small_string::SmallString::default())? {
        let br = br.read();
        if txn.name(&br) == txn.name(&channel) {
            continue;
        }
        if txn.get_changeset(txn.changes(&br), &change_id)?.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn unapply<
    T: ChannelMutTxnT + TreeMutTxnT<TreeError = <T as GraphTxnT>::GraphError>,
    C: ChangeStore,
>(
    txn: &mut T,
    channel: &mut T::Channel,
    changes: &C,
    change_id: ChangeId,
    change: &Change,
    salt: u64,
    touched_inodes: &mut TouchedInodes,
) -> Result<(), UnrecordError<C::Error, T>> {
    // `clean_inodes` is used to check whether we're seeing this file
    // for the first time.
    let mut clean_inodes = HashSet::new();
    let mut ws = Workspace::default();
    for change_ in change.changes.iter().rev().flat_map(|r| r.rev_iter()) {
        info!("unrecording {:?}", change_);
        match *change_ {
            Atom::EdgeMap(ref newedges) => {
                touched_inodes.insert((change_id, newedges.inode));
                unapply_edges(
                    changes,
                    txn,
                    T::graph_mut(channel),
                    change_id,
                    newedges,
                    &mut ws,
                )?
            }
            Atom::NewVertex(ref newvertex) => {
                touched_inodes.insert((change_id, newvertex.inode));
                if clean_inodes.insert(newvertex.inode) {
                    crate::alive::remove_forward_edges(
                        txn,
                        T::graph_mut(channel),
                        internal_pos(txn, &newvertex.inode, change_id)?,
                    )?
                }
                unapply_newvertex::<T, C>(
                    txn,
                    T::graph_mut(channel),
                    change_id,
                    &mut ws,
                    newvertex,
                )?;
            }
        }
    }

    for change in change.changes.iter().rev().flat_map(|r| r.rev_iter()) {
        match change {
            Atom::EdgeMap(n) => {
                // If we are restoring a vertex that was deleted by the
                // patch we are unrecording, remove its zombie status
                // (extra pseudo-edges) if necessary.
                remove_zombies_edges::<_, C>(txn, T::graph_mut(channel), &mut ws, change_id, n)?;
            }
            Atom::NewVertex(_) => {}
        }
    }

    for change_ in change.changes.iter().rev().flat_map(|r| r.rev_iter()) {
        match *change_ {
            Atom::EdgeMap(ref newedges) if newedges.edges.is_empty() => {}
            Atom::EdgeMap(ref newedges) if newedges.edges[0].flag.contains(EdgeFlags::FOLDER) => {
                if newedges.edges[0].flag.contains(EdgeFlags::DELETED) {
                    working_copy::undo_file_deletion(
                        txn, changes, channel, change_id, newedges, salt,
                    )?
                } else {
                    working_copy::undo_file_reinsertion::<C, _>(txn, change_id, newedges)?
                }
            }
            Atom::NewVertex(ref new_vertex)
                if new_vertex.flag.contains(EdgeFlags::FOLDER)
                    && new_vertex.down_context.is_empty() =>
            {
                working_copy::undo_file_addition(txn, change_id, new_vertex)?;
            }
            _ => {}
        }
    }

    // Check each touched inode for zombieness, and remove files that
    // aren't zombies anymore.
    debug!("touched {:?}", touched_inodes);
    for (change_id_, inode) in touched_inodes.iter() {
        if *change_id_ != change_id {
            continue;
        }
        // This inode is actually dead if its only alive adjacent
        // edges are PSEUDO|FOLDER.
        if let Ok(inode) = internal_pos(txn, inode, change_id) {
            let channel = T::graph_mut(channel);
            collect_zombies_pseudo(txn, channel, inode, &mut ws)?;
            for (v, mut e) in ws.del_edges.drain(..) {
                if e.flag().contains(EdgeFlags::PARENT) {
                    if let Ok(u) = txn.find_block_end(channel, e.dest()) {
                        e -= EdgeFlags::PARENT;
                        debug!("line {}, del {:?} {:?} {:?}", line!(), u, v, e);
                        del_graph_with_rev(txn, channel, e.flag(), *u, v, e.introduced_by())?;
                    }
                } else {
                    if let Ok(w) = txn.find_block(channel, e.dest()) {
                        debug!("line {}, del {:?} {:?} {:?}", line!(), v, w, e);
                        del_graph_with_rev(txn, channel, e.flag(), v, *w, e.introduced_by())?;
                    }
                }
            }
            let mut log = crate::apply::ZombieRepairLog::default();
            crate::apply::repair_zombies(txn, channel, inode, Some((&mut log, change_id)))?;
            remove_leftover_markings(txn, channel, change_id, &log)?;
        }
    }

    let mut inodes = crate::apply::clean_obsolete_pseudo_edges(
        txn,
        T::graph_mut(channel),
        &mut ws.apply,
        change_id,
    )?;
    debug!("inodes = {:?}", inodes);
    collect_missing_contexts(
        txn,
        txn.graph(channel),
        &mut ws.apply,
        change,
        change_id,
        &mut inodes,
    )?;
    for i in inodes {
        debug!("inodes: repair zombie {:?}", i);
        let mut log = crate::apply::ZombieRepairLog::default();
        crate::apply::repair_zombies(txn, T::graph_mut(channel), i, Some((&mut log, change_id)))?;
        remove_leftover_markings(txn, T::graph_mut(channel), change_id, &log)?;
    }
    crate::apply::repair_cyclic_paths(txn, T::graph_mut(channel), &mut ws.apply)?;
    debug!("unapply done");
    Ok(())
}

#[derive(Error)]
pub enum TouchError<WorkingCopyError: std::error::Error + 'static, T: GraphTxnT + TreeTxnT> {
    #[error(transparent)]
    Txn(#[from] TxnErr<T::GraphError>),
    #[error(transparent)]
    Tree(#[from] TreeErr<T::TreeError>),
    #[error(transparent)]
    WorkingCopy(WorkingCopyError),
}

impl<W: std::error::Error, T: GraphTxnT + TreeTxnT> std::fmt::Debug for TouchError<W, T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            TouchError::Txn(e) => std::fmt::Debug::fmt(e, fmt),
            TouchError::Tree(e) => std::fmt::Debug::fmt(e, fmt),
            TouchError::WorkingCopy(e) => std::fmt::Debug::fmt(e, fmt),
        }
    }
}

pub fn touch_inodes<
    T: ChannelMutTxnT + TreeMutTxnT<TreeError = <T as GraphTxnT>::GraphError>,
    W: WorkingCopy,
>(
    txn: &mut T,
    working_copy: &W,
    touched_inodes: &TouchedInodes,
) -> Result<(), TouchError<W::Error, T>> {
    let now = std::time::SystemTime::now();
    for (change_id, inode) in touched_inodes.iter() {
        if let Ok(inode) = internal_pos(txn, inode, *change_id)
            && let Some(inode) = txn.get_revinodes(&inode, None)?
            && let Some(name) = crate::fs::inode_filename(txn, *inode)?
        {
            debug!(
                "touching file {:?} {:?}",
                name,
                now.duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
            );
            working_copy.touch(&name, now).unwrap_or(())
        }
    }
    Ok(())
}

#[derive(Default)]
struct Workspace {
    up: HashMap<Vertex<ChangeId>, Position<Option<Hash>>>,
    down: HashMap<Vertex<ChangeId>, Position<Option<Hash>>>,
    parents: HashSet<Vertex<ChangeId>>,
    del: Vec<SerializedEdge>,
    apply: crate::apply::Workspace,
    stack: Vec<Vertex<ChangeId>>,
    del_edges: Vec<(Vertex<ChangeId>, SerializedEdge)>,
    must_reintroduce: HashSet<(Vertex<ChangeId>, Vertex<ChangeId>, ChangeId)>,
    zombies_stack: Vec<(Vertex<ChangeId>, bool, bool)>,
}

fn unapply_newvertex<T: GraphMutTxnT + TreeTxnT, C: ChangeStore>(
    txn: &mut T,
    channel: &mut T::Graph,
    change_id: ChangeId,
    ws: &mut Workspace,
    new_vertex: &NewVertex<Option<Hash>>,
) -> Result<(), UnrecordError<C::Error, T>> {
    let mut pos = Position {
        change: change_id,
        pos: new_vertex.start,
    };
    debug!("unapply_newvertex = {:?}", new_vertex);
    while let Ok(&vertex) = txn.find_block(channel, pos) {
        debug!("vertex = {:?}", vertex);
        for e in iter_adj_all(txn, channel, vertex)? {
            let e = e?;
            debug!("e = {:?}", e);
            if !e.flag().is_deleted() {
                if e.flag().is_parent() {
                    if !e.flag().is_folder() {
                        let up_v = txn.find_block_end(channel, e.dest())?;
                        ws.up.insert(*up_v, new_vertex.inode);
                    }
                } else {
                    let down_v = txn.find_block(channel, e.dest())?;
                    ws.down.insert(*down_v, new_vertex.inode);
                    if e.flag().is_folder() {
                        ws.apply.missing_context.files.insert(*down_v);
                    }
                }
            }
            ws.del.push(*e)
        }
        debug!("del = {:#?}", ws.del);
        ws.up.remove(&vertex);
        ws.down.remove(&vertex);
        ws.perform_del::<C, T>(txn, channel, vertex)?;
        if vertex.end < new_vertex.end {
            pos.pos = vertex.end
        }
    }
    Ok(())
}

impl Workspace {
    fn perform_del<C: ChangeStore, T: GraphMutTxnT + TreeTxnT>(
        &mut self,
        txn: &mut T,
        channel: &mut T::Graph,
        vertex: Vertex<ChangeId>,
    ) -> Result<(), UnrecordError<C::Error, T>> {
        for e in self.del.drain(..) {
            let (a, b) = if e.flag().is_parent() {
                (*txn.find_block_end(channel, e.dest())?, vertex)
            } else {
                (vertex, *txn.find_block(channel, e.dest())?)
            };
            debug!("line {}, del {:?} {:?} {:?}", line!(), a, b, e);
            del_graph_with_rev(
                txn,
                channel,
                e.flag() - EdgeFlags::PARENT,
                a,
                b,
                e.introduced_by(),
            )?;
        }
        Ok(())
    }
}

fn unapply_edges<T: GraphMutTxnT + TreeTxnT, P: ChangeStore>(
    changes: &P,
    txn: &mut T,
    channel: &mut T::Graph,
    change_id: ChangeId,
    newedges: &EdgeMap<Option<Hash>>,
    ws: &mut Workspace,
) -> Result<(), UnrecordError<P::Error, T>> {
    debug!("newedges = {:#?}", newedges);
    let ext: Hash = txn.get_external(&change_id).optional()?.unwrap().into();
    ws.must_reintroduce.clear();
    for n in newedges.edges.iter() {
        let mut source = crate::apply::edge::find_source_vertex(
            txn,
            channel,
            &n.from,
            change_id,
            newedges.inode,
            n.flag,
            &mut ws.apply,
        )?;
        let mut target = crate::apply::edge::find_target_vertex(
            txn,
            channel,
            &n.to,
            change_id,
            newedges.inode,
            n.flag,
            &mut ws.apply,
        )?;
        loop {
            let intro_ext = n.introduced_by.unwrap_or(ext);
            let intro = internal(txn, &n.introduced_by, change_id)?.unwrap();
            if must_reintroduce::<_, _>(
                txn, channel, changes, source, target, intro_ext, intro, change_id,
            )? {
                ws.must_reintroduce.insert((source, target, intro));
            }
            if target.end >= n.to.end {
                break;
            }
            source = target;
            target = *txn
                .find_block(channel, target.end_pos())
                .map_err(UnrecordError::from)?;
            assert_ne!(source, target);
        }
    }
    let reintro = std::mem::take(&mut ws.must_reintroduce);
    for edge in newedges.edges.iter() {
        let intro = internal(txn, &edge.introduced_by, change_id)?.unwrap();
        apply::put_newedge(
            txn,
            channel,
            &mut ws.apply,
            intro,
            newedges.inode,
            &edge.reverse(Some(ext)),
            |a, b| reintro.contains(&(a, b, intro)),
            |h| {
                if h == &ext {
                    return true;
                }
                if edge.previous.contains(EdgeFlags::DELETED) {
                    // When reintroducing an edge that was deleted,
                    // check whether the re-introduction patch knows
                    // about the alive edges around the target.
                    changes
                        .knows(edge.introduced_by.as_ref().unwrap_or(&ext), h)
                        .unwrap()
                } else {
                    // When the edge we are re-introducing is not a
                    // deletion edge, this check isn't actually used: the
                    // only zombies in that case are from a deleted
                    // context, and these aren't detected with known
                    // patches.
                    true
                }
            },
        )?;
    }

    ws.must_reintroduce = reintro;
    ws.must_reintroduce.clear();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn must_reintroduce<T: GraphTxnT + TreeTxnT, C: ChangeStore>(
    txn: &T,
    channel: &T::Graph,
    changes: &C,
    a: Vertex<ChangeId>,
    b: Vertex<ChangeId>,
    intro: Hash,
    intro_id: ChangeId,
    current_id: ChangeId,
) -> Result<bool, UnrecordError<C::Error, T>> {
    debug!("a = {:?}, b = {:?}", a, b);
    // does a patch (call it q) other than the one (p) we're
    // unrecording remove this same edge (a, b, intro) from the graph?
    // This can happen e.g. if an edge is deleted twice, in which case
    // this unrecord is likely to introduce a zombie conflict between
    // p and q.
    let b_ext = Position {
        change: txn.get_external(&b.change).optional()?.map(From::from),
        pos: b.start,
    };
    let mut stack = Vec::new();
    for e in iter_adj_all(txn, channel, a)? {
        let e = e?;
        if e.flag().contains(EdgeFlags::PARENT)
            || e.dest() != b.start_pos()
            || e.introduced_by().is_root()
            || e.introduced_by() == current_id
        {
            continue;
        }
        // Optimisation to avoid opening change files in the vast
        // majority of cases: if there is an *alive* edge `e` parallel to
        // a -> b introduced by the change that introduced a or b, don't
        // reinsert a -> b: that edge was removed when introducing `e`.
        //
        // We must not take this shortcut for a DELETED parallel edge: that
        // is a zombie marking, meaning a -> b was alive *alongside* the
        // deletion (a zombie), so it does need to be reintroduced. Fall
        // through to `edge_is_in_channel`, which decides correctly.
        if !e.flag().is_deleted() && (a.change == intro_id || b.change == intro_id) {
            return Ok(false);
        }
        stack.push(e.introduced_by())
    }
    edge_is_in_channel::<_, _>(txn, changes, b_ext, intro, &mut stack)
}

fn edge_is_in_channel<T: GraphTxnT + TreeTxnT, C: ChangeStore>(
    txn: &T,
    changes: &C,
    pos: Position<Option<Hash>>,
    introduced_by: Hash,
    stack: &mut Vec<ChangeId>,
) -> Result<bool, UnrecordError<C::Error, T>> {
    let mut visited = HashSet::new();
    while let Some(s) = stack.pop() {
        if !visited.insert(s) {
            continue;
        }
        debug!("stack: {:?}", s);
        for next in changes
            .change_deletes_position(|c| txn.get_external(&c).ok().map(From::from), s, pos)
            .map_err(UnrecordError::Changestore)?
        {
            if next == introduced_by {
                return Ok(false);
            } else if let Some(i) = txn.get_internal(&next.into())? {
                stack.push(*i)
            }
        }
    }
    Ok(true)
}

/// Remove extra pseudo-edges introduced to mark a zombie conflict, if
/// the conflict is indeed removed by this unrecord.
fn remove_zombies_edges<T: GraphMutTxnT + TreeTxnT, C: ChangeStore>(
    txn: &mut T,
    channel: &mut T::Graph,
    ws: &mut Workspace,
    change_id: ChangeId,
    newedges: &EdgeMap<Option<Hash>>,
) -> Result<(), UnrecordError<C::Error, T>> {
    debug!("remove_zombies_edges, change_id = {:?}", change_id);
    for edge in newedges.edges.iter() {
        let mut to = internal_pos(txn, &edge.to.start_pos(), change_id)?;
        loop {
            let to_block = *txn.find_block(channel, to)?;
            collect_zombies(txn, channel, change_id, to_block, ws)?;
            collect_zombies_context(txn, channel, change_id, to_block, ws)?;
            debug!("remove_zombies_edges = {:#?}", ws.del_edges);
            if to_block.end < edge.to.end {
                to = to_block.end_pos()
            } else {
                break;
            }
        }
        let from = internal_pos(txn, &edge.from, change_id)?;
        let from_block = *txn.find_block_end(channel, from)?;
        collect_zombies(txn, channel, change_id, from_block, ws)?;
        collect_zombies_context(txn, channel, change_id, from_block, ws)?;

        for (v, mut e) in ws.del_edges.drain(..) {
            if e.flag().contains(EdgeFlags::PARENT) {
                let u = *txn.find_block_end(channel, e.dest())?;
                e -= EdgeFlags::PARENT;
                debug!("line {}, del {:?} {:?} {:?}", line!(), u, v, e);
                del_graph_with_rev(txn, channel, e.flag(), u, v, e.introduced_by())?;
            } else {
                let w = *txn.find_block(channel, e.dest())?;
                debug!("line {}, del {:?} {:?} {:?}", line!(), v, w, e);
                del_graph_with_rev(txn, channel, e.flag(), v, w, e.introduced_by())?;
            }
        }
    }
    Ok(())
}

/// Collect the edges introduced by the patch we're unrecording to
/// mark zombie conflicts.
fn collect_zombies<T: GraphTxnT>(
    txn: &T,
    channel: &T::Graph,
    change_id: ChangeId,
    to_block: Vertex<ChangeId>,
    ws: &mut Workspace,
) -> Result<(), BlockError<T::GraphError>> {
    ws.stack.push(to_block);
    while let Some(v) = ws.stack.pop() {
        debug!("collect_zombies, v = {:?}", v);
        if !ws.parents.insert(v) {
            debug!("already seen");
            continue;
        }
        for e in iter_adj_all(txn, channel, v)? {
            let e = e?;
            debug!("e = {:?}", e);

            if e.introduced_by() != change_id {
                continue;
            }
            if e.flag().contains(EdgeFlags::PARENT) {
                ws.stack.push(*txn.find_block_end(channel, e.dest())?)
            } else {
                ws.stack.push(*txn.find_block(channel, e.dest())?)
            }
            if e.introduced_by() == change_id {
                ws.del_edges.push((v, *e))
            } else {
                // break
            }
        }
    }
    ws.stack.clear();
    ws.parents.clear();
    debug!("zombies collected");
    Ok(())
}

/// When `change_id` deleted `v` and `v`'s context belonged to a patch that
/// `change_id` did not know about, `apply::edge::zombify` marked that context as
/// a zombie by attaching DELETED edges (introduced by `change_id`) to it. That
/// context is reachable from the deleted vertex only through *non-BLOCK* edges
/// (BLOCK edges stay within a single change), so this walks up the non-BLOCK
/// context edges from `v` and collects the DELETED markings that `change_id`
/// left on the vertices reached. Following the alive non-BLOCK context is what
/// lets us step across the *other* change into the marked vertices — something
/// `collect_zombies` (which only follows `change_id` edges) cannot do.
fn collect_zombies_context<T: GraphTxnT>(
    txn: &T,
    channel: &T::Graph,
    change_id: ChangeId,
    v: Vertex<ChangeId>,
    ws: &mut Workspace,
) -> Result<(), BlockError<T::GraphError>> {
    let mut stack = vec![v];
    let mut visited = HashSet::new();
    while let Some(w) = stack.pop() {
        if !visited.insert(w) {
            continue;
        }
        for e in iter_adj_all(txn, channel, w)? {
            let e = e?;
            if e.flag().contains(EdgeFlags::FOLDER) {
                continue;
            }
            // A zombie marking `zombify` left on a context vertex. It always
            // writes DELETED|BLOCK edges (BLOCK asserts the vertex is dead); the
            // plain-DELETED markings `put_newedge` adds for a *later* insertion
            // landing on deleted context are ordering edges, not liveness, and
            // must not be stripped here. We only collect these on the context
            // vertices we reach, never on the deleted vertex `v` itself, which
            // zombify never marks.
            if w != v
                && e.introduced_by() == change_id
                && e.flag().is_deleted()
                && e.flag().contains(EdgeFlags::BLOCK)
            {
                ws.del_edges.push((w, *e));
            }
            // Walk up towards the marked context, but only along *alive*,
            // non-BLOCK parent edges. Those are the ordering/context edges that
            // cross into the other change; stepping across a marking edge would
            // wander into an unrelated zombie chain belonging to another change.
            if e.flag().contains(EdgeFlags::PARENT) && !e.flag().contains(EdgeFlags::BLOCK) {
                stack.push(*txn.find_block_end(channel, e.dest())?);
            }
        }
    }
    Ok(())
}

/// Collect the paths going through `to` whose vertices' adjacent
/// edges are all PSEUDO or DELETED
fn collect_zombies_pseudo<T: GraphTxnT>(
    txn: &T,
    channel: &T::Graph,
    to: Position<ChangeId>,
    ws: &mut Workspace,
) -> Result<(), BlockError<T::GraphError>> {
    // First, collect the paths downwards.
    if let Ok(to) = txn.find_block(channel, to) {
        ws.zombies_stack.push((*to, false, false))
    }

    while let Some((v, alive, on_path)) = ws.zombies_stack.pop() {
        debug!("collect_zombies_pseudo {:?} {:?} {:?}", v, alive, on_path);
        if on_path {
            // Already visited. If not alive, delete PSEUDO edges.
            if !alive {
                for e in iter_adj_all(txn, channel, v)? {
                    let e = e?;
                    if e.flag().contains(EdgeFlags::PSEUDO) {
                        ws.del_edges.push((v, *e))
                    }
                }
                if ws.zombies_stack.is_empty() {
                    debug_assert_eq!(v.start_pos(), to);
                    // Collect all the pseudo-paths up.
                    ws.parents.clear();
                    collect_zombies_up(txn, channel, to, ws)?
                }
            }
            continue;
        }

        // A vertex cannot be marked alive if it wasn't on the path.
        assert!(!alive);

        // If the vertex was already visited in another path, pass.
        if !ws.parents.insert(v) {
            continue;
        }

        ws.zombies_stack.push((v, false, true));

        // Else, iterate through all children. If any of them is
        // alive, mark the entire path as alive. Else, just push onto
        // the stack and continue the DFS.
        for e in iter_alive_children(txn, channel, v)? {
            let e = e?;
            if e.flag().intersects(EdgeFlags::PARENT | EdgeFlags::DELETED) {
                continue;
            }
            let x = txn.find_block(channel, e.dest())?;
            if is_alive(txn, channel, x)? {
                // Mark all edges on the path as alive.
                for (_, alive, on_path) in ws.zombies_stack.iter_mut() {
                    if *on_path {
                        *alive = true
                    }
                }
            } else {
                ws.zombies_stack.push((*x, false, false))
            }
        }
    }
    ws.zombies_stack.clear();
    ws.parents.clear();
    Ok(())
}

fn collect_zombies_up<T: GraphTxnT>(
    txn: &T,
    channel: &T::Graph,
    to: Position<ChangeId>,
    ws: &mut Workspace,
) -> Result<(), BlockError<T::GraphError>> {
    if let Ok(&to) = txn.find_block(channel, to) {
        ws.stack.push(to);
    }

    while let Some(v) = ws.stack.pop() {
        debug!("remove_zombies, v = {:?}", v);
        if !ws.parents.insert(v) {
            continue;
        }
        let del_len = ws.del_edges.len();
        let stack_len = ws.stack.len();
        for e in iter_adj_all(txn, channel, v)? {
            let e = e?;
            debug!("e = {:?}", e);
            if e.flag().contains(EdgeFlags::PARENT) {
                assert!(e.flag().contains(EdgeFlags::FOLDER));

                if !e.flag().intersects(EdgeFlags::PSEUDO | EdgeFlags::DELETED) {
                    // Neither a pseudo edge nor a deleted edge.
                    ws.del_edges.truncate(del_len);
                    ws.stack.truncate(stack_len);
                    break;
                }
                if let Ok(x) = txn.find_block_end(channel, e.dest()) {
                    ws.stack.push(*x)
                }
                if e.flag().contains(EdgeFlags::PSEUDO) {
                    ws.del_edges.push((v, *e))
                }
            }
        }
    }
    ws.stack.clear();
    ws.parents.clear();
    Ok(())
}

fn remove_leftover_markings<T: GraphMutTxnT + TreeTxnT>(
    txn: &mut T,
    channel: &mut T::Graph,
    change_id: ChangeId,
    log: &crate::apply::ZombieRepairLog,
) -> Result<(), TxnErr<T::GraphError>> {
    // Drop the leftover DELETED|BLOCK markings still owned by the unrecorded
    // change (collected for free during `repair_zombies`' DFS — see
    // [`crate::apply::ZombieRepairLog`]). These are down-context `zombify`
    // markings the atom-anchored removal in `unapply_edges` can't reach;
    // leaving them behind would keep a dangling reference to the change we just
    // unrecorded.
    for &(v, w) in log.leftover.iter() {
        del_graph_with_rev(
            txn,
            channel,
            EdgeFlags::DELETED | EdgeFlags::BLOCK,
            v,
            w,
            change_id,
        )?;
    }
    Ok(())
}

/// Same as crate::apply::collect_missing_contexts, but with the
/// directions reversed.
fn collect_missing_contexts<T: GraphMutTxnT + TreeTxnT>(
    txn: &T,
    channel: &T::Graph,
    ws: &mut crate::apply::Workspace,
    change: &Change,
    change_id: ChangeId,
    inodes: &mut HashSet<Position<ChangeId>>,
) -> Result<(), crate::apply::LocalApplyError<T>> {
    debug!("collect_missing_contexts");
    inodes.extend(
        ws.missing_context
            .unknown_parents
            .drain(..)
            .map(|x| internal_pos(txn, &x.2, change_id).unwrap()),
    );
    for atom in change.changes.iter().flat_map(|r| r.iter()) {
        match atom {
            // NewVertex is already collected when applying.
            Atom::NewVertex(_) => {}
            Atom::EdgeMap(n) => {
                crate::apply::has_missing_edge_context(
                    txn, channel, change_id, change, n, inodes, true,
                )?;
            }
        }
    }
    Ok(())
}
