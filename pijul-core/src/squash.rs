//! Squash several changes of a channel into a single change.
//!
//! A change is never in conflict with itself, so the squashed change is
//! produced by an actual `record`, not by composing the graph operations of
//! the squashed changes. That record runs on a throwaway *view* of the files
//! touched by the squashed changes: an in-memory working copy plus a scratch
//! tree (see [`MutTxnT::begin_scratch_tree`]). The real working copy and its
//! tables are never read nor written, so unrecorded changes stay unrecorded,
//! and repositories without a working copy can squash too.
//!
//! The operation is split in two, so that the caller can hash, sign and save
//! the new change in between:
//!
//! 1. [`squash`] checks that the squash is possible, outputs the touched files
//!    (with the squashed changes applied) into the view, refuses if they have
//!    conflicts, unrecords the squashed changes and records the view into a
//!    new, unsaved change carrying the squashed changes in its `replaces`
//!    lineage.
//! 2. [`apply_squash`] applies the saved change and moves the real working
//!    copy's inodes of files added by the squashed changes to their new
//!    positions.
//!
//! A squash is refused when:
//! - a change of the channel that is not squashed depends on a squashed one;
//! - the squashed changes leave a conflict in the files they touch (its
//!   resolution must be part of the squash);
//! - a squashed change adds a root, or the squash would record one — never
//!   allowed, since roots identify projects;
//! - the new change touches several sub-roots (independent projects), unless
//!   `force` is set;
//! - re-recording would not give back exactly the same files, which
//!   [`apply_squash`] checks (record can take lines that look like conflict
//!   markers for actual markers).
//!
//! Refusals found before unrecording (dependents, conflicts, a squashed change
//! adding a root) leave the transaction untouched. On any other error, the
//! transaction is left in an intermediate state and must be dropped without
//! committing.

use crate::HashMap;
use crate::HashSet;
use crate::change::*;
use crate::changestore::ChangeStore;
use crate::output::Conflict;
use crate::pristine::*;
use crate::record::{Builder, InodeUpdate};
use crate::small_string::SmallString;
use crate::working_copy::{Memory, WorkingCopyRead};

#[derive(Error)]
pub enum SquashError<C: std::error::Error + 'static, T: GraphTxnT + TreeTxnT> {
    #[error("Nothing to squash")]
    Empty,
    #[error("Change {} is not on the channel", hash.to_base32())]
    NotOnChannel { hash: Hash },
    #[error("Change {} is depended upon by {}, which is not squashed", hash.to_base32(), dependent.to_base32())]
    DependedUpon { hash: Hash, dependent: Hash },
    #[error("The squashed changes leave conflicts in {} file(s); record their resolution first", conflicts.len())]
    Conflicts { conflicts: Vec<Conflict> },
    #[error("{}", match hash { Some(h) => format!("Change {} adds a root, which cannot be squashed", h.to_base32()), None => "This squash would add a root, which is not allowed".to_string() })]
    AddRoot { hash: Option<Hash> },
    #[error("This squash touches {} independent roots", sub_roots.len())]
    SubRoots {
        sub_roots: Vec<crate::record::SubRoot>,
    },
    #[error("The squashed change would not give back the same {}: they may contain lines that look like conflict markers", paths.join(", "))]
    NotEquivalent { paths: Vec<String> },
    #[error("Changestore error: {0}")]
    Changestore(C),
    #[error(transparent)]
    Txn(#[from] TxnErr<T::GraphError>),
    #[error(transparent)]
    Tree(#[from] TreeErr<T::TreeError>),
    #[error(transparent)]
    Block(#[from] BlockError<T::GraphError>),
    #[error(transparent)]
    InconsistentChange(#[from] InconsistentChange<T::GraphError>),
    #[error(transparent)]
    Fs(#[from] crate::fs::FsError<T>),
    #[error(transparent)]
    File(#[from] crate::output::FileError<C, T>),
    #[error(transparent)]
    Output(#[from] crate::output::OutputError<C, T, crate::working_copy::memory::Error>),
    #[error(transparent)]
    Unrecord(#[from] crate::unrecord::UnrecordError<C, T>),
    #[error(transparent)]
    Record(#[from] crate::record::RecordError<C, crate::working_copy::memory::Error, T>),
    #[error(transparent)]
    MakeChange(#[from] MakeChangeError<T>),
    #[error(transparent)]
    LocalApply(#[from] crate::apply::LocalApplyError<T>),
}

impl<C: std::error::Error, T: GraphTxnT + TreeTxnT> std::fmt::Debug for SquashError<C, T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SquashError::Empty => write!(fmt, "Nothing to squash"),
            SquashError::NotOnChannel { hash } => {
                write!(fmt, "Change {} is not on the channel", hash.to_base32())
            }
            SquashError::DependedUpon { hash, dependent } => write!(
                fmt,
                "Change {} is depended upon by {}",
                hash.to_base32(),
                dependent.to_base32()
            ),
            SquashError::Conflicts { conflicts } => write!(fmt, "Conflicts: {:?}", conflicts),
            SquashError::AddRoot { hash } => write!(fmt, "AddRoot: {:?}", hash),
            SquashError::SubRoots { sub_roots } => write!(fmt, "SubRoots: {:?}", sub_roots),
            SquashError::NotEquivalent { paths } => write!(fmt, "NotEquivalent: {:?}", paths),
            SquashError::Changestore(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::Txn(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::Tree(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::Block(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::InconsistentChange(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::Fs(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::File(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::Output(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::Unrecord(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::Record(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::MakeChange(e) => std::fmt::Debug::fmt(e, fmt),
            SquashError::LocalApply(e) => std::fmt::Debug::fmt(e, fmt),
        }
    }
}

/// A squashed change, recorded but not yet saved nor applied. The scratch tree
/// is still active in the transaction; pass this to [`apply_squash`] once
/// `change` is saved.
pub struct Squash {
    /// The new change. Its header is the one given to [`squash`], and its
    /// lineage (`replaces`) lists the squashed changes.
    pub change: Change,
    /// The squashed changes, in application order.
    pub squashed: Vec<Hash>,
    updates: HashMap<usize, InodeUpdate>,
    /// Inode vertices introduced by the squashed changes that were alive, with
    /// their path in the view.
    added: Vec<(Position<ChangeId>, String)>,
    /// The touched files, with the squashed changes applied, and the paths they
    /// were output from.
    view: Memory,
    output_prefixes: Vec<String>,
    /// The touched inode vertices that the squashed changes did not introduce.
    kept: Vec<Position<ChangeId>>,
}

/// Record `hashes`, all on `channel`, into a single new change. `force` allows
/// the new change to span several sub-roots. See the
/// [module documentation](self).
pub fn squash<T, P>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    changes: &P,
    hashes: &[Hash],
    header: ChangeHeader,
    force: bool,
) -> Result<Squash, SquashError<P::Error, T>>
where
    T: MutTxnT + Send + Sync + 'static,
    T::Channel: Send + Sync,
    P: ChangeStore + Clone + Send + 'static,
{
    // Squashed changes, in application order.
    let mut squashed = Vec::with_capacity(hashes.len());
    {
        let txn_ = txn.read();
        let channel_ = channel.read();
        for hash in hashes {
            let id = match txn_.get_internal(&hash.into())? {
                Some(&id) => id,
                None => return Err(SquashError::NotOnChannel { hash: *hash }),
            };
            match txn_.get_changeset(txn_.changes(&channel_), &id)? {
                Some(&t) => squashed.push((u64::from(t), id, *hash)),
                None => return Err(SquashError::NotOnChannel { hash: *hash }),
            }
        }
    }
    squashed.sort();
    squashed.dedup();
    if squashed.is_empty() {
        return Err(SquashError::Empty);
    }
    let ids: HashSet<ChangeId> = squashed.iter().map(|(_, id, _)| *id).collect();

    // Every change of the channel depending on a squashed change must itself be
    // squashed, since its dependencies are about to disappear.
    {
        let txn_ = txn.read();
        let channel_ = channel.read();
        for &(_, id, hash) in squashed.iter() {
            for x in txn_.iter_revdep(&id)? {
                let (p, d) = x?;
                if *p > id {
                    break;
                }
                if !ids.contains(d) && txn_.get_changeset(txn_.changes(&channel_), d)?.is_some() {
                    return Err(SquashError::DependedUpon {
                        hash,
                        dependent: (*txn_.get_external(d).unwrap()).into(),
                    });
                }
            }
        }
    }

    let mut loaded = Vec::with_capacity(squashed.len());
    for (_, id, hash) in squashed.iter() {
        let change = changes.get_change(hash).map_err(SquashError::Changestore)?;
        if change
            .changes
            .iter()
            .any(|h| matches!(h, Hunk::AddRoot { .. }))
        {
            return Err(SquashError::AddRoot { hash: Some(*hash) });
        }
        loaded.push((*id, change));
    }

    // The files touched by the squashed changes, as positions of their inode
    // vertices.
    let (touched, new_inodes) = touched_inodes(&*txn.read(), &loaded)?;

    let mut prefixes = HashSet::default();
    paths_of(txn, channel, changes, &touched, &mut prefixes)?;

    txn.write().begin_scratch_tree()?;
    let view = Memory::new();

    // Output the touched files, with the squashed changes applied.
    debug!("squash: touched {:?}, paths {:?}", touched, prefixes);
    let mut conflicts = Vec::new();
    let output_prefixes = minimal_prefixes(&prefixes);
    for prefix in output_prefixes.iter() {
        conflicts.extend(crate::output::output_repository_no_pending_(
            &view, changes, txn, channel, prefix, true, None, 1, 0,
        )?);
    }
    // Output only follows the paths it is given, so a file with several names
    // is only seen under one of them: look for extra names in the graph.
    {
        let txn_ = txn.read();
        let channel_ = channel.read();
        let graph = txn_.graph(&*channel_);
        for pos in touched.iter() {
            let names = alive_names::<P::Error, T>(&*txn_, graph, *pos)?;
            if names.len() > 1 {
                let mut introduced_by = Vec::new();
                for n in names.iter() {
                    introduced_by.push((*txn_.get_external(&n.change).unwrap()).into())
                }
                let path = crate::fs::find_path(changes, &*txn_, &*channel_, true, *pos)?
                    .map(|p| p.path.join("/"))
                    .unwrap_or_default();
                conflicts.push(Conflict::MultipleNames {
                    path,
                    pos: [*pos],
                    names,
                    changes: introduced_by,
                });
            }
        }
    }
    if !conflicts.is_empty() {
        // Nothing has changed in the graph yet: leave the transaction as it was.
        txn.write().end_scratch_tree()?;
        return Err(SquashError::Conflicts { conflicts });
    }

    // The view, with the squashed changes applied: its tree entries, and the
    // positions of its inodes.
    let mut tree_entries = Vec::new();
    let mut inode_positions = Vec::new();
    let mut added = Vec::new();
    {
        let txn_ = txn.read();
        let start = OwnedPathId {
            parent_inode: Inode::ROOT,
            basename: SmallString::new(),
        };
        for x in txn_.iter_tree(&start, None)? {
            let (k, v) = x?;
            tree_entries.push((k.to_owned(), *v));
        }
        for x in txn_.iter_inodes()? {
            let (inode, s) = x?;
            inode_positions.push((*inode, s.position));
            if new_inodes.contains(&s.position) {
                if let Some(path) = crate::fs::inode_filename(&*txn_, *inode)? {
                    added.push((s.position, path))
                }
            }
        }
    }
    txn.write().end_scratch_tree()?;

    // Unrecord, most recent first. Unrecord keeps the tree in sync as it would
    // for a working copy, in a way that depends on the order of the unrecords
    // (a file undeleted then deleted loses its inode, then gets a new one under
    // its former name): let it do so in a throwaway tree.
    txn.write().begin_scratch_tree()?;
    for (_, _, hash) in squashed.iter().rev() {
        crate::unrecord::unrecord(
            &mut *txn.write(),
            channel,
            changes,
            hash,
            0,
            &mut Default::default(),
        )?;
    }
    txn.write().end_scratch_tree()?;

    // Rebuild the view for record: the same entries, where the inodes
    // introduced by the squashed changes have no position (record sees
    // additions), and moved files keep theirs (record sees moves)...
    txn.write().begin_scratch_tree()?;
    let mut by_pos = HashMap::default();
    {
        let mut txn_ = txn.write();
        for (k, v) in tree_entries.iter() {
            if k.basename.is_empty() {
                txn_.put_tree(k, v)?;
            } else {
                put_tree_with_rev(&mut *txn_, k, v)?;
            }
        }
        for (inode, pos) in inode_positions.iter() {
            if !ids.contains(&pos.change) {
                put_inodes_with_rev(&mut *txn_, inode, pos)?;
                by_pos.insert(*pos, *inode);
            }
        }
    }
    // ... plus the touched files the squashed changes deleted, absent from the
    // working copy of the view (record sees deletions).
    for pos in touched.iter() {
        if !ids.contains(&pos.change) && !by_pos.contains_key(pos) {
            place(txn, channel, changes, *pos, &mut by_pos)?;
        }
    }
    let mut prefixes = HashSet::default();
    {
        let txn_ = txn.read();
        let inodes = by_pos.values().chain(tree_entries.iter().map(|(_, v)| v));
        for inode in inodes {
            if let Some(path) = crate::fs::inode_filename(&*txn_, *inode)? {
                prefixes.insert(path);
            }
        }
    }

    let mut builder = Builder::new();
    // The view only holds the touched files: their siblings are not deleted.
    builder.ignore_missing = true;
    for prefix in minimal_prefixes(&prefixes) {
        builder.record(
            txn.clone(),
            crate::Algorithm::default(),
            false,
            &crate::DEFAULT_SEPARATOR,
            channel.clone(),
            &view,
            changes,
            &prefix,
            1,
        )?;
    }
    let rec = builder.finish();
    let actions = {
        let txn_ = txn.read();
        rec.actions
            .into_iter()
            .map(|a| a.globalize(&*txn_))
            .collect::<Result<Vec<_>, _>>()
            .map_err(TxnErr)?
    };
    let contents = std::mem::take(&mut *rec.contents.lock());
    let mut change =
        Change::make_change(&*txn.read(), channel, actions, contents, header, Vec::new())?;

    if change
        .changes
        .iter()
        .any(|h| matches!(h, Hunk::AddRoot { .. }))
    {
        return Err(SquashError::AddRoot { hash: None });
    }
    if !force {
        let groups = crate::record::group_by_sub_root(
            &*txn.read(),
            &*channel.read(),
            &change.hashed.changes,
            &rec.updatables,
        )?;
        if groups.len() > 1 {
            return Err(SquashError::SubRoots {
                sub_roots: groups.into_iter().map(|(sr, _)| sr).collect(),
            });
        }
    }

    // Lineage: every squashed change, and everything they already replaced.
    let mut chain = Vec::new();
    let mut seen = HashSet::default();
    for (_, c) in loaded.iter() {
        for h in c.replaces_chain() {
            if seen.insert(h) {
                chain.push(h)
            }
        }
    }
    for (_, _, h) in squashed.iter() {
        if seen.insert(*h) {
            chain.push(*h)
        }
    }
    change.hashed.set_change_metadata(&chain);

    Ok(Squash {
        change,
        squashed: squashed.into_iter().map(|(_, _, h)| h).collect(),
        updates: rec.updatables,
        added,
        view,
        output_prefixes,
        kept: touched
            .into_iter()
            .filter(|p| !ids.contains(&p.change))
            .collect(),
    })
}

/// Apply a squashed change saved under `hash` in `changes`, check that it gives
/// back the files the squashed changes gave, restore the real working-copy
/// tables, and point the inodes of files added by the squashed changes to
/// their new positions.
///
/// If the files differ ([`SquashError::NotEquivalent`]), the squash is refused
/// and the transaction must be dropped.
pub fn apply_squash<T, P>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    changes: &P,
    squash: &Squash,
    hash: &Hash,
) -> Result<(), SquashError<P::Error, T>>
where
    T: MutTxnT + Send + Sync + 'static,
    T::Channel: Send + Sync,
    P: ChangeStore + Clone + Send + 'static,
{
    crate::apply::apply_local_change(
        &mut *txn.write(),
        channel,
        &squash.change,
        hash,
        &squash.updates,
    )?;

    // New positions, read from the view before dropping it.
    let mut moved = Vec::new();
    {
        let txn_ = txn.read();
        for (old, path) in squash.added.iter() {
            if let Ok(inode) = crate::fs::find_inode(&*txn_, path) {
                if let Some(&new) = txn_.get_inodes(&inode, None)? {
                    moved.push((*old, new))
                }
            }
        }
    }
    txn.write().end_scratch_tree()?;

    // Re-recording is not always faithful: a line that looks like one of the
    // conflict markers of the version being diffed against is taken for it.
    // Output where the squashed changes put the touched files, and also where the
    // new change puts them: a file the new change moves or deletes by mistake
    // shows up as a difference.
    let mut check_paths: HashSet<String> = squash.output_prefixes.iter().cloned().collect();
    paths_of(txn, channel, changes, &squash.kept, &mut check_paths)?;
    txn.write().begin_scratch_tree()?;
    let check = Memory::new();
    let mut conflicts = Vec::new();
    for prefix in minimal_prefixes(&check_paths).iter() {
        conflicts.extend(crate::output::output_repository_no_pending_(
            &check, changes, txn, channel, prefix, true, None, 1, 0,
        )?);
    }
    txn.write().end_scratch_tree()?;
    let mut paths = different_files(&squash.view, &check);
    // The view has no conflicts, but its lines may look like the markers of a
    // conflict: the same bytes are not enough.
    for c in conflicts.iter() {
        let path = conflict_path(c);
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_string())
        }
    }
    if !paths.is_empty() {
        return Err(SquashError::NotEquivalent { paths });
    }

    // Mark the whole lineage as superseded. The squashed changes are already off
    // the channel, and so are their own predecessors.
    let mut txn_ = txn.write();
    for parent in squash.change.replaces_chain() {
        txn_.mark_superseded(&parent, hash)?;
    }

    for (old, new) in moved {
        if let Some(&inode) = txn_.get_revinodes(&old, None)? {
            txn_.del_revinodes(&old, Some(&inode))?;
            txn_.put_inodes(&inode, &new)?;
            txn_.put_revinodes(&new, &inode)?;
        }
    }
    Ok(())
}

/// Positions of the inode vertices of the files touched by `changes`, and the
/// subset of those introduced by `changes`.
#[allow(clippy::type_complexity)]
fn touched_inodes<C: std::error::Error, T: GraphTxnT + TreeTxnT>(
    txn: &T,
    changes: &[(ChangeId, Change)],
) -> Result<(Vec<Position<ChangeId>>, Vec<Position<ChangeId>>), SquashError<C, T>> {
    let mut touched = HashSet::default();
    let mut new_inodes = HashSet::default();
    for (id, change) in changes {
        let id = *id;
        for hunk in change.changes.iter() {
            for atom in hunk.iter() {
                match atom {
                    Atom::NewVertex(n) if n.flag.contains(EdgeFlags::FOLDER) => {
                        if n.start == n.end {
                            // A new inode vertex: a file addition.
                            let pos = Position {
                                change: id,
                                pos: n.start,
                            };
                            touched.insert(pos);
                            new_inodes.insert(pos);
                        }
                        // A new name for an existing inode: a move.
                        for d in n.down_context.iter() {
                            touched.insert(internal_pos(txn, d, id)?);
                        }
                    }
                    Atom::NewVertex(n) => {
                        touched.insert(internal_pos(txn, &n.inode, id)?);
                    }
                    Atom::EdgeMap(m) => {
                        let mut folder = false;
                        for e in m.edges.iter() {
                            if e.flag.contains(EdgeFlags::FOLDER) {
                                folder = true;
                                // Name edges point to inode vertices (empty);
                                // edges to name vertices are followed by an
                                // edge from that name to its inode.
                                if e.to.start == e.to.end {
                                    touched.insert(internal_pos(txn, &e.to.start_pos(), id)?);
                                }
                            }
                        }
                        if !folder {
                            touched.insert(internal_pos(txn, &m.inode, id)?);
                        }
                    }
                }
            }
        }
    }
    let mut touched: Vec<_> = touched
        .into_iter()
        .filter(|p| !p.change.is_root())
        .collect();
    touched.sort();
    let mut new_inodes: Vec<_> = new_inodes.into_iter().collect();
    new_inodes.sort();
    Ok((touched, new_inodes))
}

fn paths_of<T, P>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    changes: &P,
    positions: &[Position<ChangeId>],
    paths: &mut HashSet<String>,
) -> Result<(), SquashError<P::Error, T>>
where
    T: ChannelTxnT + TreeTxnT,
    P: ChangeStore,
{
    let txn_ = txn.read();
    let channel_ = channel.read();
    let graph = txn_.graph(&*channel_);
    for pos in positions {
        // Alive names first: the youngest name of a file may be a deleted one, and
        // outputting that path would silently leave the file out of the view.
        if let Some(p) = alive_path(&*txn_, graph, changes, *pos)? {
            paths.insert(p);
        } else if txn_.find_block_end(graph, *pos).is_ok() {
            // A deleted file: its former path still brings its directories that
            // are alive into the view.
            if let Some(p) = crate::fs::find_path(changes, &*txn_, &*channel_, true, *pos)? {
                paths.insert(p.path.join("/"));
            }
        }
    }
    Ok(())
}

/// Give the inode vertex at `pos` an entry in the view's tree, under its name
/// in the graph (or a fresh one if that name is taken), along with the entries
/// of its directories that are not in `by_pos` yet. Does nothing if `pos` has
/// no alive name.
fn place<T, P>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    changes: &P,
    pos: Position<ChangeId>,
    by_pos: &mut HashMap<Position<ChangeId>, Inode>,
) -> Result<(), SquashError<P::Error, T>>
where
    T: MutTxnT,
    P: ChangeStore,
{
    // From `pos` up to a directory that has an entry: (position, name, is_dir).
    let mut chain = Vec::new();
    let top = {
        let txn_ = txn.read();
        let channel_ = channel.read();
        let graph = txn_.graph(&*channel_);
        let f0 = EdgeFlags::FOLDER | EdgeFlags::PARENT;
        let f1 = f0 | EdgeFlags::BLOCK;
        let mut current = pos;
        loop {
            if let Some(&inode) = by_pos.get(&current) {
                break inode;
            }
            if current.change.is_root() {
                break Inode::ROOT;
            }
            let names = alive_names::<P::Error, T>(&*txn_, graph, current)?;
            let Some(name) = names.first().copied() else {
                return Ok(());
            };
            if chain.len() > 4096 {
                return Ok(());
            }
            if name.start == name.end {
                // The empty name of a root.
                chain.push((current, String::new(), true));
            } else {
                let mut buf = vec![0; name.end - name.start];
                let meta = changes
                    .get_file_meta(
                        |h| txn_.get_external(&h).ok().map(From::from),
                        name,
                        &mut buf,
                    )
                    .map_err(SquashError::Changestore)?;
                chain.push((current, meta.basename.to_string(), meta.metadata.is_dir()));
            }
            let Some(e) = iter_adjacent(&*txn_, graph, name, f0, f1)?.next() else {
                return Ok(());
            };
            let parent = *txn_.find_block_end(graph, e?.dest())?;
            current = parent.start_pos();
        }
    };

    let mut txn_ = txn.write();
    let mut parent = top;
    for (p, basename, is_dir) in chain.into_iter().rev() {
        if basename.is_empty() {
            // The inode of a root: its entries live in its parent directory.
            by_pos.insert(p, parent);
            continue;
        }
        let mut name = basename.clone();
        let mut n = 0;
        let file_id = loop {
            let id = OwnedPathId {
                parent_inode: parent,
                basename: SmallString::from_str(&name),
            };
            if txn_.get_tree(&id, None)?.is_none() {
                break id;
            }
            n += 1;
            name = format!("{}.squash-{}", basename, n);
        };
        let inode = crate::fs::create_new_inode(&mut *txn_, &file_id, 0)?;
        put_tree_with_rev(&mut *txn_, &file_id, &inode)?;
        put_inodes_with_rev(&mut *txn_, &inode, &p)?;
        if is_dir {
            let marker = OwnedPathId {
                parent_inode: inode,
                basename: SmallString::new(),
            };
            txn_.put_tree(&marker, &inode)?;
        }
        by_pos.insert(p, inode);
        parent = inode;
    }
    Ok(())
}

/// The path of the inode vertex at `pos` through alive names only, if it has
/// one.
fn alive_path<T: GraphTxnT + TreeTxnT, P: ChangeStore>(
    txn: &T,
    graph: &T::Graph,
    changes: &P,
    pos: Position<ChangeId>,
) -> Result<Option<String>, SquashError<P::Error, T>> {
    let f0 = EdgeFlags::FOLDER | EdgeFlags::PARENT;
    let f1 = f0 | EdgeFlags::BLOCK;
    let mut components = Vec::new();
    let mut current = pos;
    for _ in 0..4096 {
        if current.change.is_root() {
            components.reverse();
            return Ok(Some(components.join("/")));
        }
        let names = alive_names::<P::Error, T>(txn, graph, current)?;
        let Some(name) = names.first().copied() else {
            return Ok(None);
        };
        // The empty name of a root adds no component.
        if name.start != name.end {
            let mut buf = vec![0; name.end - name.start];
            let meta = changes
                .get_file_meta(
                    |h| txn.get_external(&h).ok().map(From::from),
                    name,
                    &mut buf,
                )
                .map_err(SquashError::Changestore)?;
            components.push(meta.basename.to_string());
        }
        let Some(e) = iter_adjacent(txn, graph, name, f0, f1)?.next() else {
            return Ok(None);
        };
        current = txn.find_block_end(graph, e?.dest())?.start_pos();
    }
    Ok(None)
}

/// The alive names of the inode vertex at `pos`.
fn alive_names<C: std::error::Error, T: GraphTxnT + TreeTxnT>(
    txn: &T,
    graph: &T::Graph,
    pos: Position<ChangeId>,
) -> Result<Vec<Vertex<ChangeId>>, SquashError<C, T>> {
    // Actual name edges only: pseudo-edges added by zombie repairs may link an
    // inode to other vertices.
    let f0 = EdgeFlags::FOLDER | EdgeFlags::PARENT;
    let f1 = f0 | EdgeFlags::BLOCK;
    let mut names = Vec::new();
    let inode = match txn.find_block_end(graph, pos) {
        Ok(v) => *v,
        Err(_) => return Ok(names),
    };
    for e in iter_adjacent(txn, graph, inode, f0, f1)? {
        let name = *txn.find_block_end(graph, e?.dest())?;
        // A name is alive if it is itself in an alive directory entry.
        if !names.contains(&name) && iter_adjacent(txn, graph, name, f0, f1)?.next().is_some() {
            names.push(name)
        }
    }
    Ok(names)
}

fn conflict_path(c: &Conflict) -> &str {
    match c {
        Conflict::Name { path, .. }
        | Conflict::ZombieFile { path, .. }
        | Conflict::MultipleNames { path, .. }
        | Conflict::Zombie { path, .. }
        | Conflict::Cyclic { path, .. }
        | Conflict::Order { path, .. } => path,
    }
}

/// The paths where two working copies differ (kind or contents).
fn different_files(a: &Memory, b: &Memory) -> Vec<String> {
    let mut paths = a.list_files();
    for p in b.list_files() {
        if !paths.contains(&p) {
            paths.push(p)
        }
    }
    paths.sort();
    let read = |m: &Memory, p: &str| {
        let meta = m.file_metadata(p).ok()?;
        let mut buf = Vec::new();
        if !meta.is_dir() {
            m.read_file(p, &mut buf).ok()?;
        }
        Some((meta.is_dir(), buf))
    };
    paths
        .into_iter()
        .filter(|p| read(a, p) != read(b, p))
        .collect()
}

/// The paths of `paths` that are not below another one.
fn minimal_prefixes(paths: &HashSet<String>) -> Vec<String> {
    let mut sorted: Vec<&String> = paths.iter().collect();
    sorted.sort();
    let mut result: Vec<String> = Vec::new();
    for p in sorted {
        if let Some(last) = result.last() {
            if last.is_empty() || p == last || p.starts_with(&format!("{}/", last)) {
                continue;
            }
        }
        result.push(p.clone())
    }
    result
}
