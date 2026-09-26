//! Hunk a change from a pristine and a working copy.
use crate::changestore::ChangeStore;
use crate::diff;
pub use crate::diff::Algorithm;
use crate::path::{Components, components};
use crate::pristine::*;
use crate::small_string::SmallString;
use crate::working_copy::WorkingCopyRead;
use crate::{HashMap, HashSet};
use crate::{alive::retrieve, text_encoding::Encoding};
use crate::{change::*, changestore::FileMetadata};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Error)]
pub enum RecordError<C: std::error::Error + 'static, W: std::error::Error, T: GraphTxnT + TreeTxnT>
{
    #[error("Changestore error: {0}")]
    Changestore(C),
    #[error("Working copy error: {0}")]
    WorkingCopy(W),
    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),
    #[error(transparent)]
    Txn(#[from] TxnErr<T::GraphError>),
    #[error(transparent)]
    Tree(#[from] TreeErr<T::TreeError>),
    #[error(transparent)]
    Diff(#[from] diff::DiffError<C, T>),
    #[error("Path not in repository: {0}")]
    PathNotInRepo(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl<C: std::error::Error, W: std::error::Error, T: GraphTxnT + TreeTxnT> std::fmt::Debug
    for RecordError<C, W, T>
{
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            RecordError::Changestore(e) => std::fmt::Debug::fmt(e, fmt),
            RecordError::WorkingCopy(e) => std::fmt::Debug::fmt(e, fmt),
            RecordError::SystemTimeError(e) => std::fmt::Debug::fmt(e, fmt),
            RecordError::Txn(e) => std::fmt::Debug::fmt(e, fmt),
            RecordError::Tree(e) => std::fmt::Debug::fmt(e, fmt),
            RecordError::Diff(e) => std::fmt::Debug::fmt(e, fmt),
            RecordError::PathNotInRepo(p) => write!(fmt, "Path not in repository: {}", p),
            RecordError::Io(e) => std::fmt::Debug::fmt(e, fmt),
        }
    }
}

impl<C: std::error::Error + 'static, W: std::error::Error + 'static, T: GraphTxnT + TreeTxnT>
    std::convert::From<crate::output::FileError<C, T>> for RecordError<C, W, T>
{
    fn from(e: crate::output::FileError<C, T>) -> Self {
        match e {
            crate::output::FileError::Changestore(e) => RecordError::Changestore(e),
            crate::output::FileError::Io(e) => RecordError::Io(e),
            crate::output::FileError::Txn(t) => RecordError::Txn(t),
        }
    }
}

/// A change in the process of being recorded. This is typically
/// created using `Builder::new`.
pub struct Builder {
    pub(crate) rec: Vec<Arc<Mutex<Recorded>>>,
    recorded_inodes: Arc<Mutex<HashMap<Inode, Position<Option<ChangeId>>>>>,
    deleted_vertices: Arc<Mutex<HashSet<Position<ChangeId>>>>,
    pub force_rediff: bool,
    pub ignore_missing: bool,
    pub contents: Arc<Mutex<Vec<u8>>>,
    new_root: Arc<Mutex<Option<NewRoot>>>,
    /// Per-inode `(mtime_ns, size, clean)` observed during this record. `clean`
    /// is `true` when the file already matched the pristine (its content diff was
    /// empty), so the entry is safe to persist even without applying the change —
    /// which is what lets `pijul diff` warm the cache. Flushed to the `inodes`
    /// stat cache so the next record can skip these files. See
    /// `notes-record-stat-cache.md`.
    stat_updates: Arc<Mutex<Vec<(Inode, u64, u64, bool)>>>,
    /// Instant the walk started; a file whose mtime is `>=` this is considered
    /// "ambiguous" (it may have changed while we were looking) and is not cached.
    walk_start: std::time::SystemTime,
    /// Declared monorepo boundaries (from the tracked `pijul.toml`, via
    /// [`Config::boundaries`]). Plain data — no config dependency in core. When
    /// non-empty, a `FileMove` whose two endpoints resolve to different
    /// boundaries is collected into [`Recorded::boundary_crossings`] so the CLI
    /// can refuse it unless `--force`.
    boundaries: Arc<Vec<String>>,
}

type NewRoot = (Position<Option<ChangeId>>, u64);

#[derive(Debug)]
struct Parent {
    basename: String,
    metadata: InodeMetadata,
    encoding: Option<Encoding>,
    parent: Position<Option<ChangeId>>,
}

/// The result of recording a change:
pub struct Recorded {
    /// The "byte contents" of the change.
    pub contents: Arc<Mutex<Vec<u8>>>,
    /// The current records, to be lated converted into change operations.
    pub actions: Vec<Hunk<Option<ChangeId>, LocalByte>>,
    /// The updates that need to be made to the ~tree~ and ~revtree~
    /// tables when this change is applied to the local repository.
    pub updatables: HashMap<usize, InodeUpdate>,
    /// The size of the largest file that was recorded in this change.
    pub largest_file: u64,
    /// Whether we have recorded binary files.
    pub has_binary_files: bool,
    /// Timestamp of the oldest changed file. If nothing changed,
    /// returns now().
    pub oldest_change: std::time::SystemTime,
    /// Redundant edges found during the comparison.
    pub redundant: Vec<crate::alive::Redundant>,
    /// Force a re-diff
    force_rediff: bool,
    deleted_vertices: Arc<Mutex<HashSet<Position<ChangeId>>>>,
    recorded_inodes: Arc<Mutex<HashMap<Inode, Position<Option<ChangeId>>>>>,
    new_root: Arc<Mutex<Option<NewRoot>>>,
    /// See [`Builder::stat_updates`].
    stat_updates: Arc<Mutex<Vec<(Inode, u64, u64, bool)>>>,
    /// See [`Builder::walk_start`].
    walk_start: std::time::SystemTime,
    /// See [`Builder::boundaries`].
    boundaries: Arc<Vec<String>>,
    /// `(old_path, new_path)` of every recorded `FileMove` that crosses a
    /// declared boundary (empty when no boundaries are configured). The CLI
    /// refuses the record unless `--force`.
    pub boundary_crossings: Vec<(String, String)>,
    /// Paths whose recorded content includes a Pijul conflict marker line
    /// (`>>>>>>> N` / `======= N` / `<<<<<<< N`). This only happens when a file
    /// carries orphaned markers the graph no longer backs — a live conflict is
    /// re-emitted by the graph and handled as structure, so its markers never
    /// reach content. The CLI refuses the record unless `--accept-conflict-markers`.
    pub conflict_marker_files: Vec<String>,
}

impl Recorded {
    /// The `(inode, mtime_ns, size, clean)` of every file whose stat was observed
    /// during the record walk. Feed these to [`update_stat_cache`]: after
    /// *applying* a change pass `clean_only = false` (every recorded file now
    /// matches the pristine); to warm the cache from a read-only walk (`pijul
    /// diff`) pass `clean_only = true`, so only files that already matched are
    /// cached and a pending edit is never hidden.
    pub fn take_stat_updates(&self) -> Vec<(Inode, u64, u64, bool)> {
        std::mem::take(&mut *self.stat_updates.lock())
    }
}

/// Persist working-copy `(mtime, size)` stats collected during [`Builder::record`]
/// into the `inodes` table's stat cache. The inodes must already exist with their
/// final positions: call it *after* `apply` for a recorded change, or on any walk
/// (e.g. `pijul diff`) for the already-tracked files it re-examined. With
/// `clean_only`, skips entries whose content differed from the pristine (only
/// meaningful when the change is *not* applied — otherwise those files would be
/// cached as clean while still dirty). See `notes-record-stat-cache.md`.
pub fn update_stat_cache<T: TreeMutTxnT>(
    txn: &mut T,
    updates: &[(Inode, u64, u64, bool)],
    clean_only: bool,
) -> Result<(), TreeErr<T::TreeError>> {
    for (inode, mtime, size, clean) in updates {
        if clean_only && !clean {
            continue;
        }
        txn.set_inode_stat(inode, *mtime, *size)?;
    }
    Ok(())
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            rec: Vec::new(),
            recorded_inodes: Arc::new(Mutex::new(HashMap::default())),
            force_rediff: false,
            ignore_missing: false,
            deleted_vertices: Arc::new(Mutex::new(HashSet::default())),
            contents: Arc::new(Mutex::new(Vec::new())),
            new_root: Arc::new(Mutex::new(None)),
            stat_updates: Arc::new(Mutex::new(Vec::new())),
            walk_start: std::time::SystemTime::UNIX_EPOCH,
            boundaries: Arc::new(Vec::new()),
        }
    }
}

impl Builder {
    /// Initialise a `Builder`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare the monorepo boundaries to check moves against (see
    /// [`Builder::boundaries`]). Call before recording; empty = no check.
    pub fn set_boundaries(&mut self, boundaries: Vec<String>) {
        self.boundaries = Arc::new(boundaries);
    }

    pub fn recorded(&mut self) -> Arc<Mutex<Recorded>> {
        let m = Arc::new(Mutex::new(self.recorded_()));
        self.rec.push(m.clone());
        m
    }

    fn recorded_(&self) -> Recorded {
        Recorded {
            contents: self.contents.clone(),
            actions: Vec::new(),
            updatables: HashMap::default(),
            largest_file: 0,
            has_binary_files: false,
            oldest_change: std::time::SystemTime::UNIX_EPOCH,
            redundant: Vec::new(),
            force_rediff: self.force_rediff,
            deleted_vertices: self.deleted_vertices.clone(),
            recorded_inodes: self.recorded_inodes.clone(),
            new_root: self.new_root.clone(),
            stat_updates: self.stat_updates.clone(),
            walk_start: self.walk_start,
            boundaries: self.boundaries.clone(),
            boundary_crossings: Vec::new(),
            conflict_marker_files: Vec::new(),
        }
    }

    /// Finish the recording.
    pub fn finish(mut self) -> Recorded {
        if self.rec.is_empty() {
            self.recorded();
        }
        let mut it = self.rec.into_iter();
        let mut result = if let Ok(rec) = Arc::try_unwrap(it.next().unwrap()) {
            rec.into_inner()
        } else {
            unreachable!()
        };
        for rec in it {
            let rec = if let Ok(rec) = Arc::try_unwrap(rec) {
                rec.into_inner()
            } else {
                unreachable!()
            };
            let off = result.actions.len();
            result.actions.extend(rec.actions);
            for (a, b) in rec.updatables {
                result.updatables.insert(a + off, b);
            }
            result.largest_file = result.largest_file.max(rec.largest_file);
            result.has_binary_files |= rec.has_binary_files;
            if result.oldest_change == std::time::UNIX_EPOCH
                || (rec.oldest_change > std::time::UNIX_EPOCH
                    && rec.oldest_change < result.oldest_change)
            {
                result.oldest_change = rec.oldest_change
            }
            result.redundant.extend(rec.redundant);
            result.boundary_crossings.extend(rec.boundary_crossings);
            result
                .conflict_marker_files
                .extend(rec.conflict_marker_files);
        }
        debug!(
            "result = {:?}, updatables = {:?}",
            result.actions, result.updatables
        );
        result
    }
}

/// An account of the files that have been added, moved or deleted, as
/// returned by record, and used by apply (when applying a change
/// created locally) to update the trees and inodes databases.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum InodeUpdate {
    Add {
        /// Inode vertex in the graph.
        pos: ChangePosition,
        /// `Inode` added by this file addition.
        inode: Inode,
    },
    Deleted {
        /// `Inode` of the deleted file.
        inode: Inode,
    },
}

impl InodeUpdate {
    /// The inode this update touches.
    pub fn inode(&self) -> Inode {
        match *self {
            InodeUpdate::Add { inode, .. } | InodeUpdate::Deleted { inode } => inode,
        }
    }
}

#[derive(Debug, Clone)]
struct RecordItem {
    v_papa: Position<Option<ChangeId>>,
    papa: Inode,
    inode: Inode,
    basename: String,
    full_path: String,
    metadata: InodeMetadata,
}

impl RecordItem {
    fn root() -> Self {
        RecordItem {
            inode: Inode::ROOT,
            papa: Inode::ROOT,
            v_papa: Position::OPTION_ROOT,
            basename: String::new(),
            full_path: String::new(),
            metadata: InodeMetadata::new(0, true),
        }
    }
}

/// Ignore inodes that are in another channel
#[allow(clippy::type_complexity)]
fn get_inodes_<T: ChannelTxnT + TreeTxnT, C: ChangeStore, W: WorkingCopyRead>(
    txn: &ArcTxn<T>,
    channel: &ChannelRef<T>,
    inode: &Inode,
) -> Result<Option<Position<ChangeId>>, RecordError<C::Error, W::Error, T>> {
    let txn = txn.read();
    let channel = channel.r.read();
    Ok(get_inodes::<_, C, W>(&*txn, &*channel, inode)?.copied())
}

#[allow(clippy::type_complexity)]
fn get_inodes<'a, T: ChannelTxnT + TreeTxnT, C: ChangeStore, W: WorkingCopyRead>(
    txn: &'a T,
    channel: &T::Channel,
    inode: &Inode,
) -> Result<Option<&'a Position<ChangeId>>, RecordError<C::Error, W::Error, T>> {
    if let Some(vertex) = txn.get_inodes(inode, None)? {
        if let Some(e) = iter_adjacent(
            txn,
            txn.graph(channel),
            vertex.inode_vertex(),
            EdgeFlags::PARENT,
            EdgeFlags::all(),
        )?
        .next()
            && e?.flag().is_parent()
        {
            return Ok(Some(vertex));
        }
        Ok(None)
    } else {
        Ok(None)
    }
}

type Task = (
    RecordItem,
    Position<ChangeId>,
    Arc<Mutex<Recorded>>,
    Option<Position<Option<ChangeId>>>,
);

struct Tasks {
    stop: bool,
    t: VecDeque<Task>,
}

/// The read-only outcome of examining one existing file during the record walk,
/// produced by [`Builder`]-level [`Recorded::prepare_existing_file`] and consumed
/// by [`Recorded::commit_existing_file`]. Splitting the per-file work this way
/// lets the heavy, read-only part (graph retrieval + diff) run for many files in
/// parallel, while the writes into the shared change stay single-threaded and in
/// walk order (so `contents` offsets are deterministic). See
/// `notes-record-stat-cache.md` §5.
struct ExistingFilePrep {
    kind: PrepKind,
}

enum PrepKind {
    /// The file is gone from the working copy: record its deletion at commit
    /// time (reads the graph, so kept out of the parallel phase for now).
    Deleted,
    Nondeleted {
        /// Whether the file's name/metadata/parent changed (or it was a zombie),
        /// i.e. whether `record_moved_file` must run at commit time.
        move_needed: bool,
        /// Encoding to hand to `record_moved_file`.
        move_encoding: Option<Encoding>,
        /// The file's mtime, captured during the walk; used to update
        /// `oldest_change` if the diff turns out non-empty.
        mtime: Option<std::time::SystemTime>,
        /// The prepared diff, if the file is a regular file whose stat cache
        /// didn't already prove it clean. `None` for directories and cache hits.
        diff: Option<crate::diff::DiffPlan>,
        /// `(mtime_ns, size)` to write to the stat cache after the diff, unless
        /// the mtime is ambiguous (in which case this is `None`).
        stat: Option<(u64, u64)>,
    },
}

impl Builder {
    #[allow(clippy::too_many_arguments)]
    pub fn record<
        T,
        W: WorkingCopyRead + Clone + Send + Sync + 'static,
        C: ChangeStore + Clone + Send + 'static,
    >(
        &mut self,
        txn: ArcTxn<T>,
        diff_algorithm: diff::Algorithm,
        stop_early: bool,
        diff_separator: &regex::bytes::Regex,
        channel: ChannelRef<T>,
        working_copy: &W,
        changes: &C,
        prefix: &str,
        n_workers: usize,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        T: ChannelMutTxnT + TreeTxnT + Send + Sync + 'static,
        T::Channel: Send + Sync,
        <W as WorkingCopyRead>::Error: 'static,
    {
        // Timestamp the start of the walk: files touched at or after this instant
        // are "ambiguous" and won't be cached as clean (see `stat_updates`).
        self.walk_start = std::time::SystemTime::now();
        let work = Arc::new(Mutex::new(Tasks {
            t: VecDeque::new(),
            stop: false,
        }));
        info!("Starting to record");
        let now = std::time::Instant::now();
        let mut stack = vec![(RecordItem::root(), components(prefix))];
        while let Some((mut item, mut components)) = stack.pop() {
            debug!("stack.pop() = Some({:?})", item);

            // Check for moves and file conflicts.
            let vertex: Option<Position<Option<ChangeId>>> =
                self.recorded_inodes.lock().get(&item.inode).cloned();

            let mut root_vertices = Vec::new();

            let vertex = if let Some(vertex) = vertex {
                vertex
            } else if item.inode == Inode::ROOT {
                debug!("TAKING LOCK {}", line!());
                let txn = txn.read();
                debug!("TAKEN");
                let channel = channel.r.read();

                // Test for a "root" vertex below the null one.
                let f0 = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
                let f1 = f0 | EdgeFlags::PSEUDO;
                self.recorded_inodes
                    .lock()
                    .insert(Inode::ROOT, Position::ROOT.to_option());
                let mut has_nonempty_root = false;
                for e in iter_adjacent(&*txn, txn.graph(&*channel), Vertex::ROOT, f0, f1)? {
                    let e = e?;
                    let child = txn.find_block(txn.graph(&*channel), e.dest()).unwrap();
                    if child.start == child.end {
                        // This is the "new" format, with multiple
                        // roots, and `grandchild` is one of the
                        // roots.
                        let grandchild =
                            iter_adjacent(&*txn, txn.graph(&*channel), *child, f0, f1)?
                                .next()
                                .unwrap()?
                                .dest();
                        root_vertices.push(grandchild);
                        self.delete_obsolete_children(
                            &*txn,
                            txn.graph(&channel),
                            working_copy,
                            changes,
                            &item.full_path,
                            grandchild,
                        )?;
                    } else {
                        // Single-root repository, we need to follow
                        // the root's children.
                        let mut name = vec![0; child.end - child.start];
                        changes
                            .get_contents(
                                |p| txn.get_external(&p).ok().map(From::from),
                                *child,
                                &mut name,
                            )
                            .map_err(RecordError::Changestore)?;
                        debug!("non-empty root {:?} {:?}", child, name);
                        has_nonempty_root = true
                    }
                }
                debug!("has_nonempty_root: {:?}", has_nonempty_root);
                debug!("root_vertices: {:?}", root_vertices);
                if has_nonempty_root && !root_vertices.is_empty() {
                    // This repository is mixed between "zero" roots,
                    // and new-style-roots.
                    root_vertices.push(Position::ROOT)
                }
                Position::ROOT.to_option()
            } else if let Some(vertex) = get_inodes_::<_, C, W>(&txn, &channel, &item.inode)? {
                if !self.dir_entries_unchanged(&txn, working_copy, &item) {
                    let txn = txn.read();
                    let channel = channel.r.read();
                    let graph = txn.graph(&*channel);
                    self.delete_obsolete_children(
                        &*txn,
                        graph,
                        working_copy,
                        changes,
                        &item.full_path,
                        vertex,
                    )?;
                }

                let rec = self.recorded();
                let new_papa = {
                    let mut recorded = self.recorded_inodes.lock();
                    recorded.insert(item.inode, vertex.to_option());
                    recorded.get(&item.papa).cloned()
                };
                let mut work = work.lock();
                work.t.push_back((item.clone(), vertex, rec, new_papa));
                std::mem::drop(work);

                vertex.to_option()
            } else {
                let rec = self.recorded();
                debug!("TAKING LOCK {}", line!());
                let mut rec = rec.lock();
                match rec.add_file(working_copy, item.clone()) {
                    Ok(Some(vertex)) => {
                        // Path addition (maybe just a single directory).
                        self.recorded_inodes.lock().insert(item.inode, vertex);
                        vertex
                    }
                    _ => continue,
                }
            };

            if root_vertices.is_empty() {
                // Move on to the next step.
                debug!("TAKING LOCK {}", line!());
                let txn = txn.read();
                let channel = channel.r.read();
                self.push_children::<_, _, C>(
                    &*txn,
                    &*channel,
                    working_copy,
                    &mut item,
                    &mut components,
                    vertex,
                    &mut stack,
                    prefix,
                    changes,
                )?;
            } else {
                for vertex in root_vertices {
                    let txn = txn.read();
                    let channel = channel.r.read();
                    if !vertex.change.is_root() {
                        let mut r = self.new_root.lock();
                        let age = txn
                            .get_changeset(txn.changes(&*channel), &vertex.change)?
                            .unwrap();
                        if let Some((_, a)) = *r {
                            if a < (*age).into() {
                                *r = Some((vertex.to_option(), (*age).into()))
                            }
                        } else {
                            *r = Some((vertex.to_option(), (*age).into()))
                        }
                    }
                    item.v_papa = vertex.to_option();
                    self.push_children::<_, _, C>(
                        &*txn,
                        &*channel,
                        working_copy,
                        &mut item,
                        &mut components,
                        vertex.to_option(),
                        &mut stack,
                        prefix,
                        changes,
                    )?;
                }
            }
        }

        info!("stop work");
        let tasks: Vec<Task> = {
            let mut work = work.lock();
            work.stop = true;
            work.t.drain(..).collect()
        };

        if n_workers <= 1 || tasks.len() <= 1 {
            // Single-threaded: prepare and commit each file in walk order.
            for (item, vertex, rec, new_papa) in &tasks {
                info!("record existing file {:?}", item);
                rec.lock().record_existing_file(
                    &txn,
                    diff_algorithm,
                    stop_early,
                    diff_separator,
                    &channel,
                    working_copy.clone(),
                    changes,
                    item,
                    *new_papa,
                    *vertex,
                )?;
            }
        } else {
            // Two-phase parallel diff (see `notes-record-stat-cache.md` §5):
            //
            //  A. Prepare each file's diff concurrently. This only *reads* the
            //     pristine (which is immutable during a record) and writes
            //     per-file `Recorded` state, so any number of files can be
            //     prepared at once. Results are stored by task index, so walk
            //     order is preserved regardless of completion order.
            //  B. Commit the prepared plans single-threaded, in walk order. This
            //     is the only writer of the shared `contents` buffer, so the byte
            //     offsets it assigns are deterministic — the resulting change is
            //     byte-for-byte identical to the single-threaded path.
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
            let n = tasks.len();
            let next = AtomicUsize::new(0);
            // Set if any prepare errored: we then fall back to a sequential pass
            // that reproduces the (typed, non-`Send`) error to return it.
            let failed = AtomicBool::new(false);
            let results: Mutex<Vec<Option<ExistingFilePrep>>> =
                Mutex::new((0..n).map(|_| None).collect());
            std::thread::scope(|s| {
                for _ in 0..n_workers.min(n) {
                    // `changes` (`&C`) is cloned to an owned handle per worker,
                    // since `ChangeStore` is only `Send`, not `Sync`. `working_copy`
                    // (`&W`) is `Sync` and shared directly.
                    let changes = (*changes).clone();
                    let (txn, channel, tasks, next, failed, results, working_copy) = (
                        &txn,
                        &channel,
                        &tasks,
                        &next,
                        &failed,
                        &results,
                        working_copy,
                    );
                    s.spawn(move || {
                        loop {
                            if failed.load(Relaxed) {
                                break;
                            }
                            let i = next.fetch_add(1, Relaxed);
                            if i >= n {
                                break;
                            }
                            let (item, vertex, rec, _) = &tasks[i];
                            let prep = rec.lock().prepare_existing_file(
                                txn,
                                diff_algorithm,
                                stop_early,
                                diff_separator,
                                channel,
                                working_copy,
                                &changes,
                                item,
                                *vertex,
                            );
                            match prep {
                                Ok(p) => results.lock()[i] = Some(p),
                                Err(_) => {
                                    failed.store(true, Relaxed);
                                    break;
                                }
                            }
                        }
                    });
                }
            });

            if failed.load(Relaxed) {
                // Rare (fatal) error path: redo sequentially to surface the error.
                for (item, vertex, rec, new_papa) in &tasks {
                    rec.lock().record_existing_file(
                        &txn,
                        diff_algorithm,
                        stop_early,
                        diff_separator,
                        &channel,
                        working_copy.clone(),
                        changes,
                        item,
                        *new_papa,
                        *vertex,
                    )?;
                }
            } else {
                for ((item, vertex, rec, new_papa), prep) in tasks.iter().zip(results.into_inner())
                {
                    rec.lock().commit_existing_file(
                        &txn,
                        diff_separator,
                        &channel,
                        working_copy,
                        changes,
                        item,
                        *new_papa,
                        *vertex,
                        prep.unwrap(),
                    )?;
                }
            }
        }

        crate::TIMERS.lock().unwrap().record += now.elapsed();

        info!("record done");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_single_thread<T, W: WorkingCopyRead + Clone, C: ChangeStore + Clone>(
        &mut self,
        txn: ArcTxn<T>,
        diff_algorithm: diff::Algorithm,
        stop_early: bool,
        diff_separator: &regex::bytes::Regex,
        channel: ChannelRef<T>,
        working_copy: &W,
        changes: &C,
        prefix: &str,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        T: ChannelMutTxnT + TreeTxnT,
        <W as WorkingCopyRead>::Error: 'static,
    {
        info!("Starting to record");
        self.walk_start = std::time::SystemTime::now();
        let now = std::time::Instant::now();
        let mut stack = vec![(RecordItem::root(), components(prefix))];
        while let Some((mut item, mut components)) = stack.pop() {
            debug!("stack.pop() = Some({:?})", item);

            // Check for moves and file conflicts.
            let vertex: Option<Position<Option<ChangeId>>> =
                self.recorded_inodes.lock().get(&item.inode).cloned();

            let mut root_vertices = Vec::new();

            let vertex = if let Some(vertex) = vertex {
                vertex
            } else if item.inode == Inode::ROOT {
                debug!("TAKING LOCK {}", line!());
                let txn = txn.read();
                debug!("TAKEN");
                let channel = channel.r.read();

                // Test for a "root" vertex below the null one.
                let f0 = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
                let f1 = f0 | EdgeFlags::PSEUDO;
                self.recorded_inodes
                    .lock()
                    .insert(Inode::ROOT, Position::ROOT.to_option());
                let mut has_nonempty_root = false;
                for e in iter_adjacent(&*txn, txn.graph(&*channel), Vertex::ROOT, f0, f1)? {
                    let e = e?;
                    let child = txn.find_block(txn.graph(&*channel), e.dest()).unwrap();
                    if child.start == child.end {
                        // This is the "new" format, with multiple
                        // roots, and `grandchild` is one of the
                        // roots.
                        let grandchild =
                            iter_adjacent(&*txn, txn.graph(&*channel), *child, f0, f1)?
                                .next()
                                .unwrap()?
                                .dest();
                        root_vertices.push(grandchild);
                        self.delete_obsolete_children(
                            &*txn,
                            txn.graph(&channel),
                            working_copy,
                            changes,
                            &item.full_path,
                            grandchild,
                        )?;
                    } else {
                        // Single-root repository, we need to follow
                        // the root's children.
                        let mut name = vec![0; child.end - child.start];
                        changes
                            .get_contents(
                                |p| txn.get_external(&p).ok().map(From::from),
                                *child,
                                &mut name,
                            )
                            .map_err(RecordError::Changestore)?;
                        debug!("non-empty root {:?} {:?}", child, name);
                        has_nonempty_root = true
                    }
                }
                debug!("has_nonempty_root: {:?}", has_nonempty_root);
                debug!("root_vertices: {:?}", root_vertices);
                if has_nonempty_root && !root_vertices.is_empty() {
                    // This repository is mixed between "zero" roots,
                    // and new-style-roots.
                    root_vertices.push(Position::ROOT)
                }
                Position::ROOT.to_option()
            } else if let Some(vertex) = get_inodes_::<_, C, W>(&txn, &channel, &item.inode)? {
                if !self.dir_entries_unchanged(&txn, working_copy, &item) {
                    let txn = txn.read();
                    let channel = channel.r.read();
                    let graph = txn.graph(&*channel);
                    self.delete_obsolete_children(
                        &*txn,
                        graph,
                        working_copy,
                        changes,
                        &item.full_path,
                        vertex,
                    )?;
                }

                let rec = self.recorded();
                let new_papa = {
                    let mut recorded = self.recorded_inodes.lock();
                    recorded.insert(item.inode, vertex.to_option());
                    recorded.get(&item.papa).cloned()
                };

                rec.lock().record_existing_file(
                    &txn,
                    diff_algorithm,
                    stop_early,
                    diff_separator,
                    &channel,
                    working_copy.clone(),
                    changes,
                    &item,
                    new_papa,
                    vertex,
                )?;

                vertex.to_option()
            } else {
                let rec = self.recorded();
                debug!("TAKING LOCK {}", line!());
                let mut rec = rec.lock();
                match rec.add_file(working_copy, item.clone()) {
                    Ok(Some(vertex)) => {
                        // Path addition (maybe just a single directory).
                        self.recorded_inodes.lock().insert(item.inode, vertex);
                        vertex
                    }
                    _ => continue,
                }
            };

            if root_vertices.is_empty() {
                // Move on to the next step.
                debug!("TAKING LOCK {}", line!());
                let txn = txn.read();
                let channel = channel.r.read();
                self.push_children::<_, _, C>(
                    &*txn,
                    &*channel,
                    working_copy,
                    &mut item,
                    &mut components,
                    vertex,
                    &mut stack,
                    prefix,
                    changes,
                )?;
            } else {
                for vertex in root_vertices {
                    let txn = txn.read();
                    let channel = channel.r.read();
                    if !vertex.change.is_root() {
                        let mut r = self.new_root.lock();
                        let age = txn
                            .get_changeset(txn.changes(&*channel), &vertex.change)?
                            .unwrap();
                        if let Some((_, a)) = *r {
                            if a < (*age).into() {
                                *r = Some((vertex.to_option(), (*age).into()))
                            }
                        } else {
                            *r = Some((vertex.to_option(), (*age).into()))
                        }
                    }
                    item.v_papa = vertex.to_option();
                    self.push_children::<_, _, C>(
                        &*txn,
                        &*channel,
                        working_copy,
                        &mut item,
                        &mut components,
                        vertex.to_option(),
                        &mut stack,
                        prefix,
                        changes,
                    )?;
                }
            }
        }
        crate::TIMERS.lock().unwrap().record += now.elapsed();
        info!("record done");
        Ok(())
    }

    /// Record the deletion of the tracked file or directory at `path` into
    /// this builder, independently of [`Builder::ignore_missing`]. This lets
    /// a caller recording from a partial working copy (with
    /// `ignore_missing` set) still delete explicitly chosen paths in the
    /// same change. `path` must be absent from `working_copy`; descendants
    /// still present there are kept, as in a regular record.
    pub fn record_deleted_path<T, W: WorkingCopyRead, C: ChangeStore>(
        &mut self,
        txn: &ArcTxn<T>,
        channel: &ChannelRef<T>,
        working_copy: &W,
        changes: &C,
        path: &str,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        T: ChannelTxnT + TreeTxnT,
        <W as WorkingCopyRead>::Error: 'static,
    {
        let txn = txn.read();
        let channel = channel.r.read();
        let inode = crate::fs::find_inode(&*txn, path).map_err(|e| match e {
            crate::fs::FsError::Tree(e) => RecordError::Tree(e),
            _ => RecordError::PathNotInRepo(path.to_string()),
        })?;
        let vertex = *get_inodes::<_, C, W>(&*txn, &*channel, &inode)?
            .ok_or_else(|| RecordError::PathNotInRepo(path.to_string()))?;
        let rec = self.recorded();
        let mut rec = rec.lock();
        rec.record_deleted_file(
            &*txn,
            txn.graph(&*channel),
            working_copy,
            path,
            vertex,
            changes,
        )
    }

    fn delete_obsolete_children<T: GraphTxnT + TreeTxnT, W: WorkingCopyRead, C: ChangeStore>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        working_copy: &W,
        changes: &C,
        full_path: &str,
        v: Position<ChangeId>,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        <W as WorkingCopyRead>::Error: 'static,
    {
        if self.ignore_missing {
            return Ok(());
        }
        let f0 = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
        let f1 = f0 | EdgeFlags::PSEUDO;
        debug!("delete_obsolete_children, v = {:?}", v);
        for child in iter_adjacent(txn, channel, v.inode_vertex(), f0, f1)? {
            let child = child?;
            debug!("find block {:?} {:?}", child.dest(), child.introduced_by());
            let child = txn.find_block(channel, child.dest()).unwrap();
            if child.start == child.end {
                // This is an empty name, i.e. the grandchild is a root vertex.
                continue;
            }
            for grandchild in iter_adjacent(txn, channel, *child, f0, f1)? {
                let grandchild = grandchild?;
                debug!("grandchild {:?}", grandchild);
                let needs_deletion =
                    if let Some(inode) = txn.get_revinodes(&grandchild.dest(), None)? {
                        debug!("inode = {:?} {:?}", inode, txn.get_revtree(inode, None));
                        if let Some(path) = crate::fs::inode_filename(txn, *inode)? {
                            working_copy.file_metadata(&path).is_err()
                        } else {
                            true
                        }
                    } else {
                        true
                    };
                if needs_deletion {
                    let mut name = vec![0; child.end - child.start];
                    changes
                        .get_contents(
                            |p| txn.get_external(&p).ok().map(From::from),
                            *child,
                            &mut name,
                        )
                        .map_err(RecordError::Changestore)?;
                    let mut full_path = full_path.to_string();
                    let meta = FileMetadata::read(&name);
                    if !full_path.is_empty() {
                        full_path.push('/');
                    }
                    full_path.push_str(meta.basename);
                    // delete recursively.
                    let rec = self.recorded();
                    let mut rec = rec.lock();
                    rec.record_deleted_file(
                        txn,
                        channel,
                        working_copy,
                        &full_path,
                        grandchild.dest(),
                        changes,
                    )?
                }
            }
        }
        Ok(())
    }

    /// Returns `true` when the working-copy directory `item` provably
    /// has the same set of entries as at the last record. Only
    /// meaningful for directories.
    fn dir_entries_unchanged<T: ChannelTxnT + TreeTxnT, W: WorkingCopyRead>(
        &self,
        txn: &ArcTxn<T>,
        working_copy: &W,
        item: &RecordItem,
    ) -> bool {
        if self.force_rediff || !item.metadata.is_dir() {
            return false;
        }
        let cur_mtime = working_copy
            .modified_time(&item.full_path)
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let walk_start_ns = self
            .walk_start
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        // Unknown (0) or ambiguous (touched within this walk) → don't trust it.
        if cur_mtime == 0 || cur_mtime >= walk_start_ns {
            return false;
        }
        let cached = txn.read().get_inode_stat(&item.inode).unwrap_or(None);
        // Refresh the cached directory mtime (size is unused for dirs → 0).
        // `clean = false`: a directory whose entries changed must be re-scanned,
        // so its mtime is only cached when the change is applied, never from a
        // read-only `pijul diff`.
        self.stat_updates
            .lock()
            .push((item.inode, cur_mtime, 0, false));
        cached == Some((cur_mtime, 0))
    }

    #[allow(clippy::too_many_arguments)]
    fn push_children<'a, T: ChannelTxnT + TreeTxnT, W: WorkingCopyRead, C: ChangeStore>(
        &mut self,
        txn: &T,
        channel: &T::Channel,
        working_copy: &W,
        item: &mut RecordItem,
        components: &mut Components<'a>,
        vertex: Position<Option<ChangeId>>,
        stack: &mut Vec<(RecordItem, Components<'a>)>,
        prefix: &str,
        changes: &C,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        <W as crate::working_copy::WorkingCopyRead>::Error: 'static,
    {
        debug!("push_children, vertex = {:?}, item = {:?}", vertex, item);
        // If `item`'s directory is the mount point of a relocated sub-root
        // (`clone --into`), its working-copy children live, in the graph, under
        // the mounted SUBROOT-INODE — reached through an empty, working-copy-less
        // sub-root NAME. Parent them there so they are not recorded as spurious
        // moves out of the sub-root. See `resolve_sub_root_mount`.
        let vertex = if let Some(change) = vertex.change {
            let resolved = resolve_sub_root_mount(
                txn,
                txn.graph(channel),
                Position {
                    change,
                    pos: vertex.pos,
                },
            )?;
            Position {
                change: Some(resolved.change),
                pos: resolved.pos,
            }
        } else {
            vertex
        };
        let comp = components.next();
        let full_path = item.full_path.clone();
        let fileid = OwnedPathId {
            parent_inode: item.inode,
            basename: SmallString::new(),
        };
        debug!("fileid = {:?}", fileid);
        let mut has_matching_children = false;
        for x in txn.iter_tree(&fileid, None)? {
            let (fileid_, child_inode) = x?;
            debug!("push_children {:?} {:?}", fileid_, child_inode);
            assert!(fileid_.parent_inode >= fileid.parent_inode);
            if fileid_.basename.is_empty() {
                continue;
            } else if fileid_.parent_inode > fileid.parent_inode {
                break;
            }
            if let Some(comp) = comp
                && comp != fileid_.basename.as_str()
            {
                continue;
            }
            has_matching_children = true;
            let basename = fileid_.basename.as_str().to_string();
            let full_path = if full_path.is_empty() {
                basename.clone()
            } else {
                full_path.clone() + "/" + &basename
            };
            debug!("fileid_ {:?} child_inode {:?}", fileid_, child_inode);
            match working_copy.file_metadata(&full_path) {
                Ok(meta) => {
                    debug!("full_path = {:?}, meta = {:?}", full_path, meta);
                    stack.push((
                        RecordItem {
                            papa: item.inode,
                            inode: *child_inode,
                            v_papa: vertex,
                            basename,
                            full_path,
                            metadata: meta,
                        },
                        components.clone(),
                    ));
                }
                _ => {
                    if let Some(vertex) = get_inodes::<_, C, W>(txn, channel, child_inode)? {
                        let rec = self.recorded();
                        let mut rec = rec.lock();
                        rec.record_deleted_file(
                            txn,
                            txn.graph(channel),
                            working_copy,
                            &full_path,
                            *vertex,
                            changes,
                        )?
                    }
                }
            }
        }
        if comp.is_some() && !has_matching_children {
            debug!("comp = {:?}", comp);
            return Err(RecordError::PathNotInRepo(prefix.to_string()));
        }
        debug!("push_children done");
        Ok(())
    }
}

impl Recorded {
    fn add_root_if_needed(
        &mut self,
        v_papa: Position<Option<ChangeId>>,
    ) -> Position<Option<ChangeId>> {
        let mut contents = self.contents.lock();
        if v_papa.change == Some(ChangeId::ROOT) {
            let mut new_root = self.new_root.lock();
            if let Some((pos, _)) = *new_root {
                pos
            } else {
                contents.push(0);
                let pos = ChangePosition(contents.len().into());
                contents.push(0);
                let pos2 = ChangePosition(contents.len().into());
                contents.push(0);
                debug!(
                    "add root if needed {:?} {:?}",
                    pos.0.as_u64(),
                    pos2.0.as_u64()
                );
                self.actions.push(Hunk::AddRoot {
                    name: Atom::NewVertex(NewVertex {
                        up_context: vec![v_papa],
                        down_context: vec![],
                        start: pos,
                        end: pos,
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        inode: v_papa,
                    }),
                    inode: Atom::NewVertex(NewVertex {
                        up_context: vec![Position { change: None, pos }],
                        down_context: vec![],
                        start: pos2,
                        end: pos2,
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        inode: v_papa,
                    }),
                });

                self.updatables.insert(
                    self.actions.len(),
                    InodeUpdate::Add {
                        inode: Inode::ROOT,
                        pos: pos2,
                    },
                );

                *new_root = Some((
                    Position {
                        change: None,
                        pos: pos2,
                    },
                    u64::MAX,
                ));
                Position {
                    change: None,
                    pos: pos2,
                }
            }
        } else {
            v_papa
        }
    }

    fn add_file<W: WorkingCopyRead>(
        &mut self,
        working_copy: &W,
        item: RecordItem,
    ) -> Result<Option<Position<Option<ChangeId>>>, W::Error> {
        debug!("record_file_addition {:?}", item);
        let meta = working_copy.file_metadata(&item.full_path)?;

        // If we're inserting at the root, add an extra "root
        // directory" empty vertex.
        let item_v_papa = self.add_root_if_needed(item.v_papa);

        let mut contents = self.contents.lock();
        contents.push(0);
        let inode_pos = ChangePosition(contents.len().into());
        contents.push(0);
        let (contents_, encoding) = if meta.is_file() {
            let start = ChangePosition(contents.len().into());
            let encoding = working_copy.decode_file(&item.full_path, &mut contents)?;
            self.has_binary_files |= encoding.is_none();
            let end = ChangePosition(contents.len().into());
            self.largest_file = self.largest_file.max(end.0.as_u64() - start.0.as_u64());
            // A newly added file bypasses the diff, so check its content for
            // conflict markers here (the bytes were just decoded, no extra pass
            // over the working copy). Same guard as the diff path: flag the file
            // so the CLI refuses it unless `--accept-conflict-markers`.
            if encoding.is_some()
                && contents[start.0.as_u64() as usize..end.0.as_u64() as usize]
                    .split(|&c| c == b'\n')
                    .any(crate::diff::is_conflict_marker_line)
            {
                self.conflict_marker_files.push(item.full_path.clone());
            }
            contents.push(0);
            if end > start {
                (
                    Some(Atom::NewVertex(NewVertex {
                        up_context: vec![Position {
                            change: None,
                            pos: inode_pos,
                        }],
                        down_context: vec![],
                        start,
                        end,
                        flag: EdgeFlags::BLOCK,
                        inode: Position {
                            change: None,
                            pos: inode_pos,
                        },
                    })),
                    encoding,
                )
            } else {
                (None, encoding)
            }
        } else {
            (None, None)
        };

        // Cache this freshly-added file's `(mtime, size)` so that the *next*
        // record (with nothing changed) can skip it. Flushed post-apply, once
        // the inode has a position. Skip ambiguous (too-recent) mtimes.
        if meta.is_file() {
            let mtime = working_copy
                .modified_time(&item.full_path)
                .ok()
                .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let walk_start = self
                .walk_start
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            if mtime != 0
                && mtime < walk_start
                && let Ok(size) = working_copy.file_size(&item.full_path)
            {
                // `clean = false`: a freshly-added file isn't in the pristine yet,
                // so it must not be cached from a read-only `pijul diff`.
                self.stat_updates
                    .lock()
                    .push((item.inode, mtime, size, false));
            }
        }

        let name_start = ChangePosition(contents.len().into());
        let file_meta = FileMetadata {
            metadata: meta,
            basename: item.basename.as_str(),
            encoding: encoding.clone(),
        };
        file_meta.write(&mut contents);
        let name_end = ChangePosition(contents.len().into());
        debug!("name start {:?} end {:?}", name_start, name_end);
        contents.push(0);
        self.actions.push(Hunk::FileAdd {
            add_name: Atom::NewVertex(NewVertex {
                up_context: vec![item_v_papa],
                down_context: vec![],
                start: name_start,
                end: name_end,
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                inode: item.v_papa,
            }),
            add_inode: Atom::NewVertex(NewVertex {
                up_context: vec![Position {
                    change: None,
                    pos: name_end,
                }],
                down_context: vec![],
                start: inode_pos,
                end: inode_pos,
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                inode: item.v_papa,
            }),
            contents: contents_,
            path: item.full_path.clone(),
            encoding,
        });
        debug!("{:?}", self.actions.last().unwrap());
        self.updatables.insert(
            self.actions.len(),
            InodeUpdate::Add {
                inode: item.inode,
                pos: inode_pos,
            },
        );
        // Return the freshly-created inode vertex for *both* files and
        // directories. The caller records it in `recorded_inodes` so a second
        // visit of the same inode is deduplicated: when ROOT has more than one
        // alive root vertex (e.g. two independent repositories merged into one
        // channel), the traversal calls `push_children` once per root and thus
        // re-walks the shared working-copy tree under `Inode::ROOT` once per
        // root. Previously files returned `None`, so their inode was never
        // marked recorded and every extra walk emitted another `FileAdd` for
        // the same path — a duplicate that then conflicts on its name.
        // `push_children` on a file inode is a no-op (a file has no tree
        // children), exactly as it already is for existing files.
        Ok(Some(Position {
            change: None,
            pos: inode_pos,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn record_existing_file<T: ChannelTxnT + TreeTxnT, W: WorkingCopyRead + Clone, C: ChangeStore>(
        &mut self,
        txn: &ArcTxn<T>,
        diff_algorithm: diff::Algorithm,
        stop_early: bool,
        diff_sep: &regex::bytes::Regex,
        channel: &ChannelRef<T>,
        working_copy: W,
        changes: &C,
        item: &RecordItem,
        new_papa: Option<Position<Option<ChangeId>>>,
        vertex: Position<ChangeId>,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        <W as crate::working_copy::WorkingCopyRead>::Error: 'static,
    {
        let prep = self.prepare_existing_file(
            txn,
            diff_algorithm,
            stop_early,
            diff_sep,
            channel,
            &working_copy,
            changes,
            item,
            vertex,
        )?;
        self.commit_existing_file(
            txn,
            diff_sep,
            channel,
            &working_copy,
            changes,
            item,
            new_papa,
            vertex,
            prep,
        )
    }

    /// Read-only half of [`Recorded::record_existing_file`]: everything that only
    /// reads the pristine and the working copy — collecting former parents,
    /// deciding move/stat, retrieving the graph and running the diff algorithm.
    /// Produces an [`ExistingFilePrep`] that [`Recorded::commit_existing_file`]
    /// later turns into hunks. Safe to run in parallel across files (it writes
    /// only per-file `Recorded` state: `redundant`, `largest_file`).
    #[allow(clippy::too_many_arguments)]
    fn prepare_existing_file<
        T: ChannelTxnT + TreeTxnT,
        W: WorkingCopyRead + Clone,
        C: ChangeStore,
    >(
        &mut self,
        txn: &ArcTxn<T>,
        diff_algorithm: diff::Algorithm,
        stop_early: bool,
        diff_sep: &regex::bytes::Regex,
        channel: &ChannelRef<T>,
        working_copy: &W,
        changes: &C,
        item: &RecordItem,
        vertex: Position<ChangeId>,
    ) -> Result<ExistingFilePrep, RecordError<C::Error, W::Error, T>>
    where
        <W as crate::working_copy::WorkingCopyRead>::Error: 'static,
    {
        debug!(
            "prepare_existing_file {:?}: {:?} {:?}",
            item.full_path, item.inode, vertex,
        );
        // Former parent(s) of vertex
        let (former_parents, is_deleted, encoding) = {
            let txn_ = txn.read();
            let channel_ = channel.read();
            collect_former_parents::<C, W, T>(changes, &*txn_, &*channel_, vertex)?
        };
        debug!(
            "prepare_existing_file: {:?} {:?} {:?}",
            item, former_parents, is_deleted,
        );

        let new_meta = match working_copy.file_metadata(&item.full_path) {
            Ok(new_meta) => new_meta,
            // The file is gone from the working copy: its deletion is recorded at
            // commit time (it reads the graph, so it stays out of this phase).
            _ => {
                return Ok(ExistingFilePrep {
                    kind: PrepKind::Deleted,
                });
            }
        };

        // Whether the name/metadata/parent changed (or the vertex is a zombie or
        // not alive in the graph): if so, `record_moved_file` must run at commit.
        let (move_needed, move_encoding) = if former_parents.is_empty() {
            // The inode exists both in the graph and in the inode tables, but
            // isn't alive in the graph (e.g. recording after applying but before
            // outputting, or outputting a tag that has this file after recording
            // its deletion).
            (true, encoding)
        } else if former_parents.len() > 1
            || former_parents[0].basename != item.basename
            || former_parents[0].metadata != item.metadata
            || former_parents[0].parent != item.v_papa
            || is_deleted
        {
            (true, former_parents[0].encoding.clone())
        } else {
            (false, None)
        };

        // Decide whether to (re-)diff this file from the per-inode stat cache
        // (format v2): skip it iff its current `(mtime, size)` exactly match the
        // values cached when it last matched the pristine, and the mtime is not
        // "ambiguous" (i.e. strictly older than the start of this walk). This is
        // the Mercurial-style dirstate check; see `notes-record-stat-cache.md`.
        let mtime = working_copy.modified_time(&item.full_path).ok();
        let cur_mtime = mtime
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let cur_size = working_copy.file_size(&item.full_path).unwrap_or(u64::MAX);
        let cached = txn.read().get_inode_stat(&item.inode).unwrap_or(None);
        let walk_start_ns = self
            .walk_start
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let ambiguous = cur_mtime == 0 || cur_mtime >= walk_start_ns;
        let up_to_date = !self.force_rediff && !ambiguous && cached == Some((cur_mtime, cur_size));

        let diff = if new_meta.is_file() && !up_to_date {
            let mut ret = {
                let txn = txn.read();
                let channel = channel.read();
                retrieve(&*txn, txn.graph(&*channel), vertex, false)?
            };
            let mut b = Vec::new();
            let encoding = working_copy
                .decode_file(&item.full_path, &mut b)
                .map_err(RecordError::WorkingCopy)?;
            debug!("diffing…");
            Some(self.diff_prepare(
                changes,
                txn,
                channel,
                diff_algorithm,
                stop_early,
                item.full_path.clone(),
                vertex.to_option(),
                &mut ret,
                b,
                encoding,
                diff_sep,
            )?)
        } else {
            None
        };
        // Whether or not the file actually differed, its content now matches the
        // pristine we recorded against, so cache its `(mtime, size)` for the next
        // record — unless the mtime is ambiguous (re-check it next time) or we
        // didn't examine the contents at all (directory / cache hit).
        let stat = if diff.is_some() && !ambiguous {
            Some((cur_mtime, cur_size))
        } else {
            None
        };

        Ok(ExistingFilePrep {
            kind: PrepKind::Nondeleted {
                move_needed,
                move_encoding,
                mtime,
                diff,
                stat,
            },
        })
    }

    /// Write-only half of [`Recorded::record_existing_file`]: turn an
    /// [`ExistingFilePrep`] into hunks. Must run single-threaded and in walk
    /// order, since it appends to the shared `contents`/`actions` and the byte
    /// offsets it assigns must be deterministic.
    #[allow(clippy::too_many_arguments)]
    fn commit_existing_file<T: ChannelTxnT + TreeTxnT, W: WorkingCopyRead + Clone, C: ChangeStore>(
        &mut self,
        txn: &ArcTxn<T>,
        diff_sep: &regex::bytes::Regex,
        channel: &ChannelRef<T>,
        working_copy: &W,
        changes: &C,
        item: &RecordItem,
        new_papa: Option<Position<Option<ChangeId>>>,
        vertex: Position<ChangeId>,
        prep: ExistingFilePrep,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        <W as crate::working_copy::WorkingCopyRead>::Error: 'static,
    {
        match prep.kind {
            PrepKind::Deleted => {
                debug!("calling record_deleted_file on {:?}", item.full_path);
                let txn_ = txn.read();
                let channel_ = channel.read();
                self.record_deleted_file(
                    &*txn_,
                    txn_.graph(&*channel_),
                    working_copy,
                    &item.full_path,
                    vertex,
                    changes,
                )?
            }
            PrepKind::Nondeleted {
                move_needed,
                move_encoding,
                mtime,
                diff,
                stat,
            } => {
                if move_needed {
                    debug!("new_papa = {:?}", new_papa);
                    let txn = txn.read();
                    let channel = channel.read();
                    self.record_moved_file::<_, _, W>(
                        changes,
                        &*txn,
                        &*channel,
                        item,
                        vertex,
                        new_papa.unwrap(),
                        move_encoding,
                    )?
                }
                if let Some(plan) = diff {
                    let len = self.actions.len();
                    self.diff_emit::<_, C>(txn, channel, &plan, item.inode, diff_sep)?;
                    // The content diff produced no hunks ⇒ the file already
                    // matched the pristine, so its stat is safe to cache even
                    // without applying this change (e.g. from `pijul diff`).
                    // Moves are emitted before `len`, so they don't count here.
                    let clean = self.actions.len() == len;
                    if !clean && let Some(last_modified) = mtime {
                        if self.oldest_change == std::time::SystemTime::UNIX_EPOCH {
                            self.oldest_change = last_modified;
                        } else {
                            self.oldest_change = self.oldest_change.min(last_modified);
                        }
                    }
                    if let Some((m, s)) = stat {
                        self.stat_updates.lock().push((item.inode, m, s, clean));
                    }
                    debug!(
                        "new actions: {:?}, total {:?}",
                        self.actions.len() - len,
                        self.actions.len()
                    );
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_moved_file<T: ChannelTxnT + TreeTxnT, C: ChangeStore, W: WorkingCopyRead>(
        &mut self,
        changes: &C,
        txn: &T,
        channel: &T::Channel,
        item: &RecordItem,
        vertex: Position<ChangeId>,
        new_papa: Position<Option<ChangeId>>,
        encoding: Option<Encoding>,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        <W as crate::working_copy::WorkingCopyRead>::Error: 'static,
    {
        debug!("record_moved_file {:?} {:?}", item, vertex);
        let basename = item.basename.as_str();
        let mut moved = collect_moved_edges::<_, _, W>(
            txn,
            changes,
            txn.graph(channel),
            new_papa,
            vertex,
            item.metadata,
            basename,
        )?;
        debug!("moved = {:#?}", moved);
        let is_resurrected = !moved.resurrect.is_empty();
        if is_resurrected {
            moved.resurrect.append(&mut moved.alive);
            if !moved.need_new_name {
                moved.resurrect.append(&mut moved.edges);
            }
            self.actions.push(Hunk::FileUndel {
                undel: Atom::EdgeMap(EdgeMap {
                    edges: moved.resurrect,
                    inode: item.v_papa,
                }),
                contents: None,
                path: item.full_path.clone(),
                encoding: encoding.clone(),
            });
        }

        let item_v_papa = if !moved.edges.is_empty() && moved.need_new_name {
            self.add_root_if_needed(item.v_papa)
        } else {
            item.v_papa
        };

        let mut contents = self.contents.lock();
        contents.push(0);
        let meta_start = ChangePosition(contents.len().into());
        FileMetadata {
            metadata: item.metadata,
            basename,
            encoding: encoding.clone(),
        }
        .write(&mut contents);
        let meta_end = ChangePosition(contents.len().into());
        contents.push(0);
        if !moved.edges.is_empty() {
            // If there was exactly one alive name, this is a regular
            // move, i.e. not a conflict.
            if moved.n_alive_names == 1 || (moved.need_new_name && !is_resurrected) {
                debug!("need_new_name {:?}", item.v_papa);
                let add = if moved.need_new_name && !is_resurrected {
                    moved.edges.append(&mut moved.alive);
                    Atom::NewVertex(NewVertex {
                        up_context: vec![item_v_papa],
                        down_context: vec![vertex.to_option()],
                        start: meta_start,
                        end: meta_end,
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        inode: item_v_papa,
                    })
                } else {
                    Atom::EdgeMap(EdgeMap {
                        edges: moved.alive,
                        inode: item_v_papa,
                    })
                };
                // `find_path` is evaluated against the *current* (pre-apply)
                // graph, where `vertex` still sits at its old location: this is
                // the OLD full path. `item.full_path` is the working-copy path,
                // i.e. the NEW one. If the two resolve to different declared
                // boundaries, the move crosses one — collect it for the CLI's
                // `--force` gate (both endpoints are only available here).
                let old_path = crate::fs::find_path(changes, txn, channel, true, vertex)?
                    .unwrap()
                    .path
                    .join("/");
                if !self.boundaries.is_empty()
                    && boundary_of(&old_path, &self.boundaries)
                        != boundary_of(&item.full_path, &self.boundaries)
                {
                    self.boundary_crossings
                        .push((old_path.clone(), item.full_path.clone()));
                }
                self.actions.push(Hunk::FileMove {
                    del: Atom::EdgeMap(EdgeMap {
                        edges: moved.edges,
                        inode: item.v_papa,
                    }),
                    add,
                    path: old_path,
                });
            } else {
                self.actions.push(Hunk::SolveNameConflict {
                    name: Atom::EdgeMap(EdgeMap {
                        edges: moved.edges,
                        inode: item.v_papa,
                    }),
                    path: item.full_path.clone(),
                });
                contents.truncate(meta_start.0.as_usize())
            }
        } else {
            contents.truncate(meta_start.0.as_usize())
        }
        Ok(())
    }

    pub fn take_updatables(&mut self) -> HashMap<usize, InodeUpdate> {
        std::mem::take(&mut self.updatables)
    }

    pub fn into_change<T: ChannelTxnT + DepsTxnT<DepsError = <T as GraphTxnT>::GraphError>>(
        self,
        txn: &T,
        channel: &ChannelRef<T>,
        header: crate::change::ChangeHeader,
    ) -> Result<crate::change::LocalChangeLocal, crate::change::MakeChangeError<T>> {
        let actions = self
            .actions
            .into_iter()
            .map(|rec| rec.globalize(txn).unwrap())
            .collect();
        let contents = if let Ok(c) = Arc::try_unwrap(self.contents) {
            c.into_inner()
        } else {
            unreachable!()
        };
        crate::change::LocalChange::make_change(txn, channel, actions, contents, header, Vec::new())
    }
}

#[allow(clippy::type_complexity)]
fn collect_former_parents<C: ChangeStore, W: WorkingCopyRead, T: ChannelTxnT + TreeTxnT>(
    changes: &C,
    txn: &T,
    channel: &T::Channel,
    vertex: Position<ChangeId>,
) -> Result<(Vec<Parent>, bool, Option<Encoding>), RecordError<C::Error, W::Error, T>>
where
    W::Error: 'static,
{
    let mut former_parents = Vec::new();
    let f0 = EdgeFlags::FOLDER | EdgeFlags::PARENT;
    let f1 = EdgeFlags::all();
    // True iff the file's NAME edge is itself DELETED (a zombie/deleted entry);
    // set below when such an edge is seen. It must start `false`: an alive file
    // whose name/parent/metadata are unchanged is *not* moved. (Initialising it
    // to `true` forced `move_needed` for every existing file — harmless when
    // `record_moved_file` is idempotent, but for a relocated sub-root file it
    // re-links across the working-copy-less passthrough and records a spurious
    // move, flattening the sub-root.)
    let mut is_deleted = false;
    let mut encoding_ = None;
    for name_ in iter_adjacent(txn, txn.graph(channel), vertex.inode_vertex(), f0, f1)? {
        debug!("name_ = {:?}", name_);
        let name_ = name_?;
        if !name_.flag().contains(EdgeFlags::PARENT) {
            debug!("continue");
            continue;
        }

        let name_dest = txn
            .find_block_end(txn.graph(channel), name_.dest())
            .unwrap();
        let mut meta = vec![0; name_dest.end - name_dest.start];
        let FileMetadata {
            basename,
            metadata,
            encoding,
        } = changes
            .get_file_meta(
                |p| txn.get_external(&p).ok().map(From::from),
                *name_dest,
                &mut meta,
            )
            .map_err(RecordError::Changestore)?;
        debug!(
            "former basename of {:?}: {:?} {:?}",
            vertex, basename, metadata
        );

        if name_.flag().contains(EdgeFlags::DELETED) {
            debug!("is_deleted {:?}", name_);
            is_deleted = true;
            if encoding_.is_none() {
                encoding_ = encoding
            }
            break;
        }
        if let Some(v_papa) = iter_adjacent(txn, txn.graph(channel), *name_dest, f0, f1)?.next() {
            let v_papa = v_papa?;
            if !v_papa.flag().contains(EdgeFlags::DELETED) {
                if encoding_.is_none() {
                    encoding_ = encoding.clone()
                }
                former_parents.push(Parent {
                    basename: basename.to_string(),
                    metadata,
                    encoding,
                    parent: v_papa.dest().to_option(),
                })
            }
        }
    }
    Ok((former_parents, is_deleted, encoding_))
}

#[derive(Debug)]
struct MovedEdges {
    edges: Vec<NewEdge<Option<ChangeId>>>,
    alive: Vec<NewEdge<Option<ChangeId>>>,
    resurrect: Vec<NewEdge<Option<ChangeId>>>,
    need_new_name: bool,
    n_alive_names: usize,
}

fn collect_moved_edges<T: GraphTxnT + TreeTxnT, C: ChangeStore, W: WorkingCopyRead>(
    txn: &T,
    changes: &C,
    channel: &T::Graph,
    parent_pos: Position<Option<ChangeId>>,
    current_pos: Position<ChangeId>,
    new_meta: InodeMetadata,
    name: &str,
) -> Result<MovedEdges, RecordError<C::Error, W::Error, T>>
where
    <W as crate::working_copy::WorkingCopyRead>::Error: 'static,
{
    debug!("collect_moved_edges {:?}", current_pos);
    let mut moved = MovedEdges {
        edges: Vec::new(),
        alive: Vec::new(),
        resurrect: Vec::new(),
        need_new_name: true,
        n_alive_names: 0,
    };
    let mut del_del = HashMap::default();
    let mut alive = HashMap::default();
    let mut previous_name = Vec::new();
    let mut last_alive_meta = None;
    let mut is_first_parent = true;
    for parent in iter_adjacent(
        txn,
        channel,
        current_pos.inode_vertex(),
        EdgeFlags::FOLDER | EdgeFlags::PARENT,
        EdgeFlags::all(),
    )? {
        let parent = parent?;
        if !parent
            .flag()
            .contains(EdgeFlags::FOLDER | EdgeFlags::PARENT)
        {
            continue;
        }
        debug!("parent = {:?}", parent);
        let mut parent_was_resurrected = false;
        if !parent.flag().contains(EdgeFlags::PSEUDO) {
            if parent.flag().contains(EdgeFlags::DELETED) {
                debug!("resurrecting parent");
                moved.resurrect.push(NewEdge {
                    previous: parent.flag() - EdgeFlags::PARENT,
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    from: parent.dest().to_option(),
                    to: current_pos.inode_vertex().to_option(),
                    introduced_by: Some(parent.introduced_by()),
                });
                parent_was_resurrected = true;
                let v = alive
                    .entry((parent.dest(), current_pos.inode_vertex()))
                    .or_insert_with(HashSet::new);
                v.insert(None);
            } else {
                let v = alive
                    .entry((parent.dest(), current_pos.inode_vertex()))
                    .or_insert_with(HashSet::new);
                v.insert(Some(parent.introduced_by()));
            }
        }
        debug!("parent_was_resurrected: {:?}", parent_was_resurrected);
        let parent_dest = txn.find_block_end(channel, parent.dest()).unwrap();
        previous_name.resize(parent_dest.end - parent_dest.start, 0);
        let FileMetadata {
            metadata: parent_meta,
            basename: parent_name,
            ..
        } = changes
            .get_file_meta(
                |p| txn.get_external(&p).ok().map(From::from),
                *parent_dest,
                &mut previous_name,
            )
            .map_err(RecordError::Changestore)?;
        debug!(
            "parent_dest {:?} {:?} {:?} {:?}",
            parent_dest, parent_meta, parent_name, name
        );
        let name_changed = parent_name != name;
        let mut meta_changed = new_meta != parent_meta;
        if cfg!(windows)
            && !meta_changed
            && let Some(m) = last_alive_meta
        {
            meta_changed = new_meta != m
        }
        let mut name_is_alive = false;
        for grandparent in iter_adjacent(
            txn,
            channel,
            *parent_dest,
            EdgeFlags::FOLDER | EdgeFlags::PARENT,
            EdgeFlags::all(),
        )? {
            let grandparent = grandparent?;
            if !grandparent
                .flag()
                .contains(EdgeFlags::FOLDER | EdgeFlags::PARENT)
                || grandparent.flag().contains(EdgeFlags::PSEUDO)
            {
                continue;
            }
            debug!("grandparent: {:?}", grandparent);
            let grandparent_dest = txn.find_block_end(channel, grandparent.dest()).unwrap();
            assert_eq!(grandparent_dest.start, grandparent_dest.end);
            debug!(
                "grandparent_dest {:?} {:?}, parent_pos = {:?}",
                grandparent_dest,
                std::str::from_utf8(&previous_name[2..]),
                parent_pos,
            );
            let grandparent_changed = if parent_pos.change == Some(ChangeId::ROOT) {
                // Because repos may have multiple roots there may be
                // a mismatch here.  The "no change" case when
                // `parent_pos` is a root vertex is when
                // `grandparent.dest()` is also a root vertex.
                !is_root_vertex(txn, channel, grandparent.dest())?
            } else {
                parent_pos != grandparent.dest().to_option()
            };
            debug!(
                "change = {:?} {:?} {:?}",
                grandparent_changed, name_changed, meta_changed
            );
            if !grandparent.flag().contains(EdgeFlags::DELETED) {
                name_is_alive = true
            }
            if grandparent.flag().contains(EdgeFlags::DELETED) {
                if !grandparent_changed && !name_changed && !meta_changed {
                    // We resurrect the name
                    (if parent_was_resurrected {
                        &mut moved.resurrect
                    } else {
                        &mut moved.alive
                    })
                    .push(NewEdge {
                        previous: grandparent.flag() - EdgeFlags::PARENT,
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        from: grandparent.dest().to_option(),
                        to: parent_dest.to_option(),
                        introduced_by: Some(grandparent.introduced_by()),
                    });
                    if !parent_was_resurrected && !parent.flag().contains(EdgeFlags::PSEUDO) {
                        moved.alive.push(NewEdge {
                            previous: parent.flag() - EdgeFlags::PARENT,
                            flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                            from: parent.dest().to_option(),
                            to: current_pos.inode_vertex().to_option(),
                            introduced_by: Some(parent.introduced_by()),
                        })
                    }
                    moved.need_new_name = false;
                    // We've found an alive parent, delete the others.
                    is_first_parent = false;
                } else {
                    // Clean up the extra deleted edges.
                    debug!("cleanup");
                    let v = del_del
                        .entry((grandparent.dest(), parent_dest))
                        .or_insert_with(HashSet::new);
                    v.insert(Some(grandparent.introduced_by()));
                }
            } else if grandparent_changed
                || name_changed
                || (meta_changed && cfg!(unix))
                || !is_first_parent
            {
                if !parent.flag().contains(EdgeFlags::PSEUDO) {
                    moved.edges.push(NewEdge {
                        previous: parent.flag() - EdgeFlags::PARENT - EdgeFlags::PSEUDO,
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED,
                        from: grandparent.dest().to_option(),
                        to: parent_dest.to_option(),
                        introduced_by: Some(grandparent.introduced_by()),
                    });
                    // The following extra edge is meant to allow
                    // detection of missing contexts in folders: indeed,
                    // if we didn't have it, we couldn't tell the
                    // difference between a convergent renaming or
                    // deletion and a conflict between a renaming and a
                    // deletion.\
                    if !parent_was_resurrected {
                        moved.alive.push(NewEdge {
                            previous: parent.flag() - EdgeFlags::PARENT,
                            flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                            from: parent.dest().to_option(),
                            to: current_pos.inode_vertex().to_option(),
                            introduced_by: Some(parent.introduced_by()),
                        })
                    }
                }
            } else {
                last_alive_meta = Some(new_meta);
                let v = alive
                    .entry((grandparent.dest(), *parent_dest))
                    .or_insert_with(HashSet::new);
                v.insert(Some(grandparent.introduced_by()));
                moved.need_new_name = false;
                // We've found an alive parent, delete the others.
                is_first_parent = false;
            }
        }
        if name_is_alive {
            moved.n_alive_names += 1
        }
    }

    for ((from, to), intro) in del_del {
        if intro.len() > 1 {
            for introduced_by in intro {
                if introduced_by.is_some() {
                    moved.edges.push(NewEdge {
                        previous: EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED,
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED,
                        from: from.to_option(),
                        to: to.to_option(),
                        introduced_by,
                    })
                }
            }
        }
    }

    debug!("alive = {:#?}", alive);

    for ((from, to), intro) in alive {
        if intro.len() > 1 || !moved.resurrect.is_empty() {
            for introduced_by in intro {
                if introduced_by.is_some() {
                    moved.alive.push(NewEdge {
                        previous: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        from: from.to_option(),
                        to: to.to_option(),
                        introduced_by,
                    })
                }
            }
        }
    }

    Ok(moved)
}

/// Is this vertex a (potentially deleted) "root vertex", i.e. a root
/// of the file hierarchy?
fn is_root_vertex<T: GraphTxnT>(
    txn: &T,
    channel: &T::Graph,
    v: Position<ChangeId>,
) -> Result<bool, TxnErr<T::GraphError>> {
    for parent in iter_adjacent(
        txn,
        channel,
        v.inode_vertex(),
        EdgeFlags::FOLDER | EdgeFlags::PARENT,
        EdgeFlags::all(),
    )? {
        let parent = parent?;
        if !parent.flag().contains(EdgeFlags::PARENT) {
            continue;
        }
        let p = parent.dest();
        let p = txn.find_block_end(channel, p).unwrap();
        if p.start == p.end {
            return Ok(true);
        } else {
            return Ok(false);
        }
    }
    Ok(false)
}

impl Recorded {
    fn record_deleted_file<T: GraphTxnT + TreeTxnT, W: WorkingCopyRead, C: ChangeStore>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        working_copy: &W,
        full_path: &str,
        current_vertex: Position<ChangeId>,
        changes: &C,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        <W as WorkingCopyRead>::Error: 'static,
    {
        debug!("record_deleted_file {:?} {:?}", current_vertex, full_path);
        let mut stack = vec![(current_vertex.inode_vertex(), None)];
        let mut visited = HashSet::default();
        let mut full_path = std::borrow::Cow::Borrowed(full_path);
        while let Some((vertex, inode)) = stack.pop() {
            debug!("vertex {:?}, inode {:?}", vertex, inode);
            if let Some(path) = tree_path(txn, &vertex.start_pos())? {
                if working_copy.file_metadata(&path).is_ok() {
                    debug!("not deleting {:?}", path);
                    continue;
                }
                full_path = path.into()
            }

            // Kill this vertex
            if let Some(inode) = inode {
                self.delete_file_edge(txn, channel, vertex, inode)?
            } else if vertex.start == vertex.end {
                debug!("delete_recursively {:?}", vertex);
                // Killing an inode.
                {
                    let mut deleted_vertices = self.deleted_vertices.lock();
                    if !deleted_vertices.insert(vertex.start_pos()) {
                        continue;
                    }
                }
                if let Some(inode) = txn.get_revinodes(&vertex.start_pos(), None)? {
                    debug!(
                        "delete_recursively, vertex = {:?}, inode = {:?}",
                        vertex, inode
                    );
                    self.recorded_inodes
                        .lock()
                        .insert(*inode, vertex.start_pos().to_option());
                    self.updatables.insert(
                        self.actions.len() + 1,
                        InodeUpdate::Deleted { inode: *inode },
                    );
                }
                self.delete_inode_vertex::<_, _, W>(
                    changes,
                    txn,
                    channel,
                    vertex,
                    vertex.start_pos(),
                    &full_path,
                )?
            }

            // Move on to the descendants.
            for edge in iter_adjacent(
                txn,
                channel,
                vertex,
                EdgeFlags::empty(),
                EdgeFlags::all() - EdgeFlags::DELETED - EdgeFlags::PARENT,
            )? {
                let edge = edge?;
                debug!("delete_recursively, edge: {:?}", edge);
                let dest = txn
                    .find_block(channel, edge.dest())
                    .expect("delete_recursively, descendants");
                let inode = if inode.is_some() {
                    assert!(!edge.flag().contains(EdgeFlags::FOLDER));
                    inode
                } else if edge.flag().contains(EdgeFlags::FOLDER) {
                    None
                } else {
                    assert_eq!(vertex.start, vertex.end);
                    Some(vertex.start_pos())
                };
                if visited.insert(edge.dest()) {
                    stack.push((*dest, inode))
                }
            }
        }
        Ok(())
    }

    fn delete_inode_vertex<T: GraphTxnT + TreeTxnT, C: ChangeStore, W: WorkingCopyRead>(
        &mut self,
        changes: &C,
        txn: &T,
        channel: &T::Graph,
        vertex: Vertex<ChangeId>,
        inode: Position<ChangeId>,
        path: &str,
    ) -> Result<(), RecordError<C::Error, W::Error, T>>
    where
        <W as WorkingCopyRead>::Error: 'static,
    {
        debug!("delete_inode_vertex {:?}", path);
        let mut edges = Vec::new();
        let mut enc = None;
        let mut previous_name = Vec::new();
        for parent in iter_adjacent(
            txn,
            channel,
            vertex,
            EdgeFlags::FOLDER | EdgeFlags::PARENT,
            EdgeFlags::all(),
        )? {
            let parent = parent?;
            if !parent.flag().contains(EdgeFlags::PARENT) {
                continue;
            }
            assert!(parent.flag().contains(EdgeFlags::FOLDER));
            let parent_dest = txn.find_block_end(channel, parent.dest()).unwrap();
            if enc.is_none() {
                previous_name.resize(parent_dest.end - parent_dest.start, 0);
                let FileMetadata { encoding, .. } = changes
                    .get_file_meta(
                        |p| txn.get_external(&p).ok().map(From::from),
                        *parent_dest,
                        &mut previous_name,
                    )
                    .map_err(RecordError::Changestore)?;
                enc = Some(encoding);
            }

            for grandparent in iter_adjacent(
                txn,
                channel,
                *parent_dest,
                EdgeFlags::FOLDER | EdgeFlags::PARENT,
                EdgeFlags::all(),
            )? {
                let grandparent = grandparent?;
                if !grandparent.flag().contains(EdgeFlags::PARENT)
                    || grandparent.flag().contains(EdgeFlags::PSEUDO)
                {
                    continue;
                }
                assert!(grandparent.flag().contains(EdgeFlags::PARENT));
                assert!(grandparent.flag().contains(EdgeFlags::FOLDER));
                edges.push(NewEdge {
                    previous: grandparent.flag() - EdgeFlags::PARENT,
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from: grandparent.dest().to_option(),
                    to: parent_dest.to_option(),
                    introduced_by: Some(grandparent.introduced_by()),
                });
            }
            if !parent.flag().contains(EdgeFlags::PSEUDO) {
                edges.push(NewEdge {
                    previous: parent.flag() - EdgeFlags::PARENT,
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from: parent.dest().to_option(),
                    to: vertex.to_option(),
                    introduced_by: Some(parent.introduced_by()),
                });
            }
        }
        debug!("deleting {:?}", edges);
        if !edges.is_empty() {
            self.actions.push(Hunk::FileDel {
                del: Atom::EdgeMap(EdgeMap {
                    edges,
                    inode: inode.to_option(),
                }),
                contents: None,
                path: path.to_string(),
                encoding: enc.unwrap(),
            })
        }
        Ok(())
    }

    fn delete_file_edge<T: GraphTxnT>(
        &mut self,
        txn: &T,
        channel: &T::Graph,
        to: Vertex<ChangeId>,
        inode: Position<ChangeId>,
    ) -> Result<(), TxnErr<T::GraphError>> {
        if let Some(Hunk::FileDel { contents, .. }) = self.actions.last_mut() {
            if contents.is_none() {
                *contents = Some(Atom::EdgeMap(EdgeMap {
                    edges: Vec::new(),
                    inode: inode.to_option(),
                }))
            }
            if let Some(Atom::EdgeMap(mut e)) = contents.take() {
                for parent in iter_adjacent(
                    txn,
                    channel,
                    to,
                    EdgeFlags::PARENT,
                    EdgeFlags::all() - EdgeFlags::DELETED,
                )? {
                    let parent = parent?;
                    if parent.flag().contains(EdgeFlags::PSEUDO) {
                        continue;
                    }
                    assert!(parent.flag().contains(EdgeFlags::PARENT));
                    assert!(!parent.flag().contains(EdgeFlags::FOLDER));
                    e.edges.push(NewEdge {
                        previous: parent.flag() - EdgeFlags::PARENT,
                        flag: (parent.flag() - EdgeFlags::PARENT) | EdgeFlags::DELETED,
                        from: parent.dest().to_option(),
                        to: to.to_option(),
                        introduced_by: Some(parent.introduced_by()),
                    })
                }
                if !e.edges.is_empty() {
                    *contents = Some(Atom::EdgeMap(e))
                }
            }
        } else {
            unreachable!()
        }
        Ok(())
    }
}

/// The declared boundary that owns `path`: the longest entry of `boundaries`
/// that is a path-**component** prefix of `path` (or equal to it). Returns
/// `None` when `path` lies in no declared boundary — the residual/unowned
/// "root" zone, which counts as a distinct zone for crossing purposes (so
/// moving a file out of a boundary into unowned territory crosses too).
///
/// Component-aware: `libs/foo` owns `libs/foo` and `libs/foo/x`, but **not**
/// `libs/foobar`. Longest-match makes nesting work: with `[libs, libs/foo]`,
/// `libs/foo/x` resolves to `libs/foo`, not `libs`.
///
/// A move crosses iff `boundary_of(old) != boundary_of(new)` — symmetric by
/// construction, so both directions (child→ancestor and ancestor→child) are
/// caught. Never implement this as an asymmetric containment test.
pub fn boundary_of<'a>(path: &str, boundaries: &'a [String]) -> Option<&'a str> {
    let mut best: Option<&'a str> = None;
    for b in boundaries {
        let prefix = b.trim_matches('/');
        if prefix.is_empty() {
            continue;
        }
        let matches = path == prefix
            || (path.len() > prefix.len()
                && path.starts_with(prefix)
                && path.as_bytes()[prefix.len()] == b'/');
        if matches && best.map_or(true, |cur| prefix.len() > cur.len()) {
            best = Some(prefix);
        }
    }
    best
}

/// Which sub-root a recorded hunk belongs to (see
/// [`crate::pristine::owning_sub_root`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubRoot {
    /// An existing sub-root, identified by its INODE vertex.
    Existing(Position<ChangeId>),
    /// The single brand-new sub-root created by this very change (an `AddRoot`
    /// hunk, or a new root-level entry in a repo that has no sub-root yet — the
    /// legacy zero-root layout). A working-copy record creates at most one new
    /// sub-root (`add_root_if_needed` memoises it), so all new-sub-root hunks
    /// collapse into this one group — otherwise a first record of a fresh
    /// project, whose `AddRoot` and new files each yield a distinct new inode,
    /// would be misreported as touching several sub-roots. Note that once a
    /// sub-root exists, a *new top-level file* resolves to that existing
    /// sub-root (see [`inode_sub_root`]), not to this variant.
    New,
}

/// The sub-root a single (globalized) hunk touches.
fn hunk_sub_root<T, L>(
    txn: &T,
    channel: &T::Channel,
    idx: usize,
    hunk: &Hunk<Option<Hash>, L>,
    updatables: &HashMap<usize, InodeUpdate>,
) -> Result<SubRoot, TxnErr<T::GraphError>>
where
    T: ChannelTxnT + TreeTxnT<TreeError = <T as GraphTxnT>::GraphError>,
{
    // All atoms of a hunk share one inode; the first is representative.
    if let Some(inode_pos) = hunk.iter().next().map(|a| a.inode()) {
        if let Some(h) = inode_pos.change {
            if let Some(cid) = txn.get_internal(&h.into())? {
                let gpos = Position {
                    change: *cid,
                    pos: inode_pos.pos,
                };
                if let Some(inode) = txn.get_revinodes(&gpos, None).map_err(|e| TxnErr(e.0))? {
                    let inode = *inode;
                    if let Some(sr) = owning_sub_root(txn, txn.graph(channel), inode)? {
                        return Ok(SubRoot::Existing(sr));
                    }
                }
            }
        }
    }
    // Newly-added inode: climb the tree until we hit one already in the graph,
    // and use its sub-root. If we reach the root, it's a brand-new sub-root.
    //
    // The `InodeUpdate` describing hunk `idx` is keyed on `idx + 1`, not `idx`:
    // both `Recorded::add_file` and `record_deleted_file` insert it as
    // `actions.len()` / `actions.len() + 1` immediately around pushing their
    // hunk, so `key == hunk_index + 1` (see the same invariant used by the
    // `--split-per-root` update partition). Reading `&idx` fetched the previous
    // hunk's update, misclassifying every added file by its neighbour.
    if let Some(InodeUpdate::Add { inode, .. }) = updatables.get(&(idx + 1)) {
        return inode_sub_root(txn, channel, *inode);
    }
    Ok(SubRoot::New)
}

/// The sub-root an inode belongs to, resolving both existing inodes (via the
/// graph) and freshly-added ones (by climbing the `tree` table to the nearest
/// ancestor already in the graph). When the climb reaches [`Inode::ROOT`], the
/// inode is a new top-level entry: in the multi-root layout it belongs to the
/// existing root sub-root (via [`crate::pristine::owning_sub_root`], which maps
/// `Inode::ROOT` to its passthrough INODE), and only in the legacy zero-root
/// layout — where root has no owning sub-root — is it a brand-new sub-root.
pub fn inode_sub_root<T>(
    txn: &T,
    channel: &T::Channel,
    inode: Inode,
) -> Result<SubRoot, TxnErr<T::GraphError>>
where
    T: ChannelTxnT + TreeTxnT<TreeError = <T as GraphTxnT>::GraphError>,
{
    let mut cur = inode;
    let mut seen = HashSet::default();
    loop {
        if !seen.insert(cur) {
            return Ok(SubRoot::New);
        }
        if txn
            .get_inodes(&cur, None)
            .map_err(|e| TxnErr(e.0))?
            .is_some()
        {
            if let Some(sr) = owning_sub_root(txn, txn.graph(channel), cur)? {
                return Ok(SubRoot::Existing(sr));
            }
            return Ok(SubRoot::New);
        }
        match txn.get_revtree(&cur, None).map_err(|e| TxnErr(e.0))? {
            Some(pathid) if !pathid.parent_inode.is_root() => cur = pathid.parent_inode,
            // The tree parent is the repo root: climb to `Inode::ROOT` and let
            // the next iteration resolve it. In the multi-root layout
            // `get_inodes(Inode::ROOT)` is the top sub-root's INODE, so a new
            // top-level entry is attributed to that existing project; in the
            // legacy zero-root layout root owns no sub-root and it stays `New`.
            Some(_) => cur = Inode::ROOT,
            None => return Ok(SubRoot::New),
        }
    }
}

/// Group hunk indices by the sub-root each touches, preserving first-seen
/// order. `hunks.len()` distinct sub-roots means the record spans that many
/// independent projects; the returned groups partition the hunk indices so a
/// caller can emit one commuting change per sub-root.
pub fn group_by_sub_root<T, L>(
    txn: &T,
    channel: &T::Channel,
    hunks: &[Hunk<Option<Hash>, L>],
    updatables: &HashMap<usize, InodeUpdate>,
) -> Result<Vec<(SubRoot, Vec<usize>)>, TxnErr<T::GraphError>>
where
    T: ChannelTxnT + TreeTxnT<TreeError = <T as GraphTxnT>::GraphError>,
{
    let mut order: Vec<SubRoot> = Vec::new();
    let mut groups: HashMap<SubRoot, Vec<usize>> = HashMap::default();
    for (idx, hunk) in hunks.iter().enumerate() {
        let sr = hunk_sub_root(txn, channel, idx, hunk, updatables)?;
        groups
            .entry(sr)
            .or_insert_with(|| {
                order.push(sr);
                Vec::new()
            })
            .push(idx);
    }
    Ok(order
        .into_iter()
        .map(|s| {
            let v = groups.remove(&s).unwrap();
            (s, v)
        })
        .collect())
}

/// Error type for [`relocate_sub_root`].
#[derive(Error)]
pub enum RelocateError<T: GraphTxnT> {
    #[error(transparent)]
    Txn(#[from] TxnErr<T::GraphError>),
    #[error("Not a live sub-root name vertex (no alive FOLDER edge to ROOT)")]
    NotASubRoot,
    #[error("Building relocation change: {0}")]
    MakeChange(#[from] crate::change::MakeChangeError<T>),
}

impl<T: GraphTxnT> std::fmt::Debug for RelocateError<T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            RelocateError::Txn(e) => std::fmt::Debug::fmt(e, fmt),
            RelocateError::NotASubRoot => write!(fmt, "NotASubRoot"),
            RelocateError::MakeChange(e) => std::fmt::Debug::fmt(e, fmt),
        }
    }
}

/// Build a change that relocates the sub-root whose (empty) NAME vertex is
/// `name_vertex` into a freshly-created directory `dir_name`, itself a new
/// child of the inode vertex `dest_parent` (typically the current channel's
/// own sub-root INODE, so the relocated project ends up under `dir_name/`).
///
/// The relocation is a [`Hunk::FileMove`] that deletes the alive `ROOT → NAME`
/// folder edge and re-introduces the *same* NAME vertex under the new
/// directory's inode. Keeping the same NAME vertex (rather than minting a fresh
/// one) is what preserves the sub-root signature: after the move the NAME
/// retains a `DELETED | FOLDER | PARENT` edge to ROOT, so
/// `is_sub_root_name(.., require_relocated = true)` and `owning_sub_root` keep
/// recognising it. See the `monorepo-sub-roots` campaign node.
///
/// `dest_parent` must be an inode vertex whose own NAME is *not* a direct child
/// of ROOT (i.e. it must itself live under some sub-root), otherwise `dir_name`
/// would spuriously satisfy `is_sub_root_name`. Callers pass the current
/// channel's sub-root INODE for this.
pub fn relocate_sub_root<T>(
    txn: &T,
    channel: &ChannelRef<T>,
    dest_parent: Position<ChangeId>,
    name_vertex: Vertex<ChangeId>,
    dir_name: &str,
    header: ChangeHeader,
) -> Result<Change, RelocateError<T>>
where
    T: ChannelTxnT + DepsTxnT<DepsError = <T as GraphTxnT>::GraphError>,
{
    // Locate the alive `ROOT → NAME` folder edge we are about to delete. It is
    // stored on `name_vertex` as a `FOLDER | PARENT` edge whose `dest()` is the
    // ROOT position.
    // (flag, introduced_by) of the alive `ROOT → NAME` edge.
    let (root_edge_flag, root_edge_intro) = {
        let ch = channel.read();
        let graph = txn.graph(&*ch);
        let mut found = None;
        for e in iter_adjacent(
            txn,
            graph,
            name_vertex,
            EdgeFlags::FOLDER | EdgeFlags::PARENT,
            EdgeFlags::all(),
        )? {
            let e = e?;
            if !e.flag().is_parent() || !e.flag().is_folder() || e.flag().is_deleted() {
                continue;
            }
            if e.dest() == Position::ROOT {
                found = Some((e.flag(), e.introduced_by()));
                break;
            }
        }
        found.ok_or(RelocateError::NotASubRoot)?
    };

    // Contents buffer: the new directory's inode marker byte, then its
    // `FileMetadata` (name + dir metadata), mirroring `Recorded::add_file`.
    let mut contents = Vec::new();
    contents.push(0);
    let dir_inode_pos = ChangePosition(contents.len().into());
    contents.push(0);
    let name_start = ChangePosition(contents.len().into());
    FileMetadata {
        // Must match what the working copy reports for a directory:
        // `file_metadata` normalises dir permissions to `perm & 0o100`, so a
        // freshly output directory always reads back as `new(0o100, true)`.
        // Using `0o755` here made the mount directory's recorded metadata differ
        // from the working copy, so the next record saw a spurious dir move.
        metadata: InodeMetadata::new(0o100, true),
        basename: dir_name,
        encoding: None,
    }
    .write(&mut contents);
    let name_end = ChangePosition(contents.len().into());
    contents.push(0);

    let dir_inode_local = Position {
        change: None,
        pos: dir_inode_pos,
    };

    // Hunk 1: create the directory `dir_name` as a child of `dest_parent`.
    let add_dir: Hunk<Option<ChangeId>, LocalByte> = Hunk::FileAdd {
        add_name: Atom::NewVertex(NewVertex {
            up_context: vec![dest_parent.to_option()],
            down_context: vec![],
            start: name_start,
            end: name_end,
            flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
            inode: dest_parent.to_option(),
        }),
        add_inode: Atom::NewVertex(NewVertex {
            up_context: vec![Position {
                change: None,
                pos: name_end,
            }],
            down_context: vec![],
            start: dir_inode_pos,
            end: dir_inode_pos,
            flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
            inode: dest_parent.to_option(),
        }),
        contents: None,
        path: dir_name.to_string(),
        encoding: None,
    };

    // Hunk 2: reparent the sub-root NAME vertex under the new directory.
    //
    // `del` deletes the alive `ROOT → NAME` edge (turning it into a
    // `DELETED | FOLDER | PARENT` edge, the relocation signature). `add`
    // re-introduces the same NAME vertex as a child of `dir_inode`. Since no
    // `dir_inode → NAME` edge existed before, we model the introduction as a
    // resurrection (`previous = DELETED`): apply's `del_graph_with_rev` is then
    // a no-op and `put_graph_with_rev` adds the alive edge, while the reverse
    // (unrecord) correctly deletes it again.
    let del_flag = root_edge_flag - EdgeFlags::PARENT - EdgeFlags::PSEUDO;
    let move_hunk: Hunk<Option<ChangeId>, LocalByte> = Hunk::FileMove {
        del: Atom::EdgeMap(EdgeMap {
            edges: vec![NewEdge {
                previous: del_flag,
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED,
                from: Position::ROOT.to_option(),
                to: name_vertex.to_option(),
                introduced_by: Some(root_edge_intro),
            }],
            inode: dir_inode_local,
        }),
        add: Atom::EdgeMap(EdgeMap {
            edges: vec![NewEdge {
                previous: EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED,
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                from: dir_inode_local,
                to: name_vertex.to_option(),
                introduced_by: None,
            }],
            inode: dir_inode_local,
        }),
        path: dir_name.to_string(),
    };

    let hunks = vec![
        add_dir.globalize(txn).map_err(|e| TxnErr(e))?,
        move_hunk.globalize(txn).map_err(|e| TxnErr(e))?,
    ];

    Ok(Change::make_change(
        txn,
        channel,
        hunks,
        contents,
        header,
        Vec::new(),
    )?)
}
