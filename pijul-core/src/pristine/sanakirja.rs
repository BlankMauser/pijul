use super::*;
use crate::HashMap;
use ::sanakirja::*;
use parking_lot::Mutex;
use std::collections::hash_map::Entry;
#[cfg(feature = "mmap")]
use std::path::Path;
use std::sync::Arc;

/// A Sanakirja pristine.
pub struct Pristine {
    pub env: Arc<::sanakirja::Env>,
}

pub(crate) type P<K, V> = btree::page::Page<K, V>;
pub type Db<K, V> = btree::Db<K, V>;
pub(crate) type UP<K, V> = btree::page_unsized::Page<K, V>;
pub type UDb<K, V> = btree::Db_<K, V, UP<K, V>>;

#[derive(Debug, Error)]
pub enum SanakirjaError {
    #[error(transparent)]
    Sanakirja(#[from] ::sanakirja::Error),
    #[error("Pristine locked")]
    PristineLocked,
    #[error("Pristine corrupt")]
    PristineCorrupt,
    #[error(transparent)]
    Borrow(#[from] std::cell::BorrowError),
    #[error("Cannot dropped a borrowed channel: {:?}", c)]
    ChannelRc { c: String },
    #[error("Pristine version mismatch. Cloning over the network can fix this.")]
    Version,
}

impl std::convert::From<::sanakirja::CRCError> for SanakirjaError {
    fn from(_: ::sanakirja::CRCError) -> Self {
        SanakirjaError::PristineCorrupt
    }
}

impl std::convert::From<::sanakirja::CRCError> for TxnErr<SanakirjaError> {
    fn from(_: ::sanakirja::CRCError) -> Self {
        TxnErr(SanakirjaError::PristineCorrupt)
    }
}

impl std::convert::From<::sanakirja::Error> for TxnErr<SanakirjaError> {
    fn from(e: ::sanakirja::Error) -> Self {
        TxnErr(e.into())
    }
}

impl std::convert::From<::sanakirja::Error> for TreeErr<SanakirjaError> {
    fn from(e: ::sanakirja::Error) -> Self {
        TreeErr(e.into())
    }
}

impl std::convert::From<TxnErr<::sanakirja::Error>> for TxnErr<SanakirjaError> {
    fn from(e: TxnErr<::sanakirja::Error>) -> Self {
        TxnErr(e.0.into())
    }
}

impl std::convert::From<TreeErr<::sanakirja::Error>> for TreeErr<SanakirjaError> {
    fn from(e: TreeErr<::sanakirja::Error>) -> Self {
        TreeErr(e.0.into())
    }
}

impl Pristine {
    #[cfg(feature = "mmap")]
    pub fn new<P: AsRef<Path>>(name: P) -> Result<Self, SanakirjaError> {
        Self::new_with_size(name, 1 << 20)
    }

    /// # Safety
    ///
    /// Multiple processes might be writing to the same memory-mapped
    /// file, leading to corruption. If you use this, make sure you
    /// have another locking mechanism in place on the file.
    #[cfg(feature = "mmap")]
    pub unsafe fn new_nolock<P: AsRef<Path>>(name: P) -> Result<Self, SanakirjaError> {
        unsafe { Self::new_with_size_nolock(name, 1 << 20) }
    }

    #[cfg(feature = "mmap")]
    pub fn new_with_size<P: AsRef<Path>>(name: P, size: u64) -> Result<Self, SanakirjaError> {
        let env = ::sanakirja::Env::new(name, size, 2);
        match env {
            Ok(env) => Ok(Pristine { env: Arc::new(env) }),
            Err(::sanakirja::Error::IO(e)) => {
                if let std::io::ErrorKind::WouldBlock = e.kind() {
                    Err(SanakirjaError::PristineLocked)
                } else {
                    Err(SanakirjaError::Sanakirja(::sanakirja::Error::IO(e)))
                }
            }
            Err(e) => Err(SanakirjaError::Sanakirja(e)),
        }
    }

    /// # Safety
    ///
    /// Same as new_nolock:
    ///
    /// Multiple processes might be writing to the same memory-mapped
    /// file, leading to corruption. If you use this, make sure you
    /// have another locking mechanism in place on the file.
    #[cfg(feature = "mmap")]
    pub unsafe fn new_with_size_nolock<P: AsRef<Path>>(
        name: P,
        size: u64,
    ) -> Result<Self, SanakirjaError> {
        unsafe {
            Ok(Pristine {
                env: Arc::new(::sanakirja::Env::new_nolock(name, size, 2)?),
            })
        }
    }
    pub fn new_anon() -> Result<Self, SanakirjaError> {
        Self::new_anon_with_size(1 << 20)
    }
    pub fn new_anon_with_size(size: u64) -> Result<Self, SanakirjaError> {
        Ok(Pristine {
            env: Arc::new(::sanakirja::Env::new_anon(size, 2)?),
        })
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
#[repr(usize)]
pub enum Root {
    Version,
    Tree,
    RevTree,
    Inodes,
    RevInodes,
    Internal,
    External,
    RevDep,
    Channels,
    TouchedFiles,
    Dep,
    RevTouchedFiles,
    Partials,
    Remotes,
    Approvals,
    Git0,
    Git1,
    /// Obsolescence markers: `SerializedHash` (a change superseded by an amend)
    /// -> `SerializedHash` (its superseder). Written by `record --amend`, cleared
    /// by `unrecord`, consulted by `pull` to skip re-introducing a predecessor
    /// that a local amend already replaced. Opened on demand (like `Approvals`),
    /// so a pre-existing pristine without it stays readable — it's created on
    /// first write.
    Superseded,
}

const VERSION: u64 = 2u64;

impl Pristine {
    pub fn txn_begin(&self) -> Result<Txn, SanakirjaError> {
        {
            let txn = ::sanakirja::Env::txn_begin(self.env.clone())?;
            let version = txn.root(Root::Version as usize);
            if version != VERSION {
                std::mem::drop(txn);
                if version == 1 {
                    // A read-only transaction can't migrate; do it in a mutable
                    // one (rebuilds `inodes` to format v2) and commit before
                    // opening the read transaction. See `mut_txn_begin`.
                    self.mut_txn_begin()?.commit()?;
                } else {
                    return Err(SanakirjaError::Version);
                }
            }
        }
        let txn = ::sanakirja::Env::txn_begin(self.env.clone())?;
        debug!("txn_begin");
        fn begin(txn: ::sanakirja::Txn<Arc<::sanakirja::Env>>) -> Option<Txn> {
            Some(Txn {
                channels: txn.root_db(Root::Channels as usize)?,
                external: txn.root_db(Root::External as usize)?,
                internal: txn.root_db(Root::Internal as usize)?,
                inodes: txn.root_db(Root::Inodes as usize)?,
                revinodes: txn.root_db(Root::RevInodes as usize)?,
                tree: txn.root_db(Root::Tree as usize)?,
                revtree: txn.root_db(Root::RevTree as usize)?,
                revdep: txn.root_db(Root::RevDep as usize)?,
                touched_files: txn.root_db(Root::TouchedFiles as usize)?,
                rev_touched_files: txn.root_db(Root::RevTouchedFiles as usize)?,
                partials: txn.root_db(Root::Partials as usize)?,
                dep: txn.root_db(Root::Dep as usize)?,
                remotes: txn.root_db(Root::Remotes as usize)?,
                // Absent on a pre-feature pristine: `None`, not an error.
                superseded: txn.root_db(Root::Superseded as usize),
                open_channels: Mutex::new(HashMap::default()),
                open_remotes: Mutex::new(HashMap::default()),
                txn,
                counter: 0,
                cur_channel: None,
            })
        }
        debug!("txn begin done");
        match begin(txn) {
            Some(txn) => Ok(txn),
            _ => Err(SanakirjaError::PristineCorrupt),
        }
    }

    pub fn arc_txn_begin(
        &self,
    ) -> Result<ArcTxn<MutTxn<::sanakirja::MutTxn<Arc<::sanakirja::Env>>>>, SanakirjaError> {
        Ok(ArcTxn(Arc::new(RwLock::new(self.mut_txn_begin()?))))
    }

    pub fn mut_txn_begin(
        &self,
    ) -> Result<MutTxn<::sanakirja::MutTxn<Arc<::sanakirja::Env>>>, SanakirjaError> {
        unsafe {
            let mut txn = ::sanakirja::Env::mut_txn_begin(self.env.clone()).unwrap();
            let migrate_v1 = match txn.root(Root::Version as usize) {
                Some(v) if v == VERSION => false,
                Some(1) => true,
                Some(_) => return Err(SanakirjaError::Version),
                None => {
                    txn.set_root(Root::Version as usize, VERSION);
                    false
                }
            };
            // Format v1 -> v2: the `inodes` value grew from `Position` to
            // `SerializedInode`, adding a working-copy `(mtime, size)` cache used
            // by `record` to skip unchanged files. Rebuild the table with
            // `mtime = 0` ("unknown") so the first record after the upgrade
            // re-reads each file once, then caches it. Silent and automatic;
            // this branch can be removed once no pre-v2 pristine is expected
            // (roughly post-1.0). Old table pages are left unreferenced (a small
            // one-time leak) rather than freed, to keep the migration simple.
            let inodes: Db<Inode, SerializedInode> = if migrate_v1 {
                let mut entries = Vec::new();
                let old: Option<Db<Inode, Position<ChangeId>>> = txn.root_db(Root::Inodes as usize);
                if let Some(old) = old {
                    for x in btree::iter(&txn, &old, None)? {
                        let (k, v) = x?;
                        entries.push((*k, *v));
                    }
                }
                let mut new: Db<Inode, SerializedInode> = btree::create_db_(&mut txn)?;
                for (k, v) in entries {
                    btree::put(&mut txn, &mut new, &k, &SerializedInode::new(v))?;
                }
                // `new` is stored in the `inodes` field below and persisted to
                // `Root::Inodes` by `commit`; only the version needs setting here.
                txn.set_root(Root::Version as usize, VERSION);
                new
            } else if let Some(db) = txn.root_db(Root::Inodes as usize) {
                db
            } else {
                btree::create_db_(&mut txn)?
            };
            Ok(MutTxn {
                channels: if let Some(db) = txn.root_db(Root::Channels as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                external: if let Some(db) = txn.root_db(Root::External as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                internal: if let Some(db) = txn.root_db(Root::Internal as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                inodes,
                revinodes: if let Some(db) = txn.root_db(Root::RevInodes as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                tree: if let Some(db) = txn.root_db(Root::Tree as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                revtree: if let Some(db) = txn.root_db(Root::RevTree as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                revdep: if let Some(db) = txn.root_db(Root::RevDep as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                dep: if let Some(db) = txn.root_db(Root::Dep as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                touched_files: if let Some(db) = txn.root_db(Root::TouchedFiles as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                rev_touched_files: if let Some(db) = txn.root_db(Root::RevTouchedFiles as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                partials: if let Some(db) = txn.root_db(Root::Partials as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                remotes: if let Some(db) = txn.root_db(Root::Remotes as usize) {
                    db
                } else {
                    btree::create_db_(&mut txn)?
                },
                // Created lazily by `mark_superseded`, so a repo that never amends
                // never grows the table.
                superseded: txn.root_db(Root::Superseded as usize),
                open_channels: Mutex::new(HashMap::default()),
                open_remotes: Mutex::new(HashMap::default()),
                txn,
                counter: 0,
                cur_channel: None,
            })
        }
    }
}

pub type Txn = GenericTxn<::sanakirja::Txn<Arc<::sanakirja::Env>>>;
pub type MutTxn<T> = GenericTxn<T>;
pub type MutTxn0 = GenericTxn<::sanakirja::MutTxn<Arc<::sanakirja::Env>>>;
pub trait RawMutTxnT:
    ::sanakirja::AllocPage<Error = ::sanakirja::Error>
    + ::sanakirja::RootPageMut
    + ::sanakirja::Commit
    + ::sanakirja::LoadPage<Error = ::sanakirja::Error>
{
}

impl<
    T: ::sanakirja::AllocPage<Error = ::sanakirja::Error>
        + ::sanakirja::RootPageMut
        + ::sanakirja::Commit
        + ::sanakirja::LoadPage<Error = ::sanakirja::Error>,
> RawMutTxnT for T
{
}

/// A transaction, used both for mutable and immutable transactions,
/// depending on type parameter `T`.
///
/// In Sanakirja, both `sanakirja::Txn` and `sanakirja::MutTxn`
/// implement `sanakirja::Transaction`, explaining our implementation
/// of `TxnT` for `Txn<T>` for all `T: sanakirja::Transaction`. This
/// covers both mutable and immutable transactions in a single
/// implementation.
pub struct GenericTxn<T: ::sanakirja::LoadPage<Error = ::sanakirja::Error> + ::sanakirja::RootPage>
{
    #[doc(hidden)]
    pub txn: T,
    #[doc(hidden)]
    pub internal: UDb<SerializedHash, ChangeId>,
    #[doc(hidden)]
    pub external: UDb<ChangeId, SerializedHash>,
    pub inodes: Db<Inode, SerializedInode>,
    pub revinodes: Db<Position<ChangeId>, Inode>,

    pub tree: UDb<PathId, Inode>,
    pub revtree: UDb<Inode, PathId>,

    revdep: Db<ChangeId, ChangeId>,
    dep: Db<ChangeId, ChangeId>,

    touched_files: Db<Position<ChangeId>, ChangeId>,
    rev_touched_files: Db<ChangeId, Position<ChangeId>>,

    partials: UDb<SmallStr, Position<ChangeId>>,
    channels: UDb<SmallStr, SerializedChannel>,
    remotes: UDb<RemoteId, SerializedRemote>,
    // Obsolescence markers: superseded change -> its amend. `None` when the repo
    // predates the feature; created lazily by the first `mark_superseded`.
    superseded: Option<UDb<SerializedHash, SerializedHash>>,

    pub(crate) open_channels: Mutex<HashMap<SmallString, ChannelRef<Self>>>,
    open_remotes: Mutex<HashMap<RemoteId, RemoteRef<Self>>>,
    counter: usize,
    cur_channel: Option<String>,
}

direct_repr!(SerializedPublicKey);

/// This is actually safe because the only non-Send fields are
/// `open_channels` and `open_remotes`, but we can't do anything with
/// a `ChannelRef` whose transaction has been moved to another thread.
unsafe impl<T: ::sanakirja::LoadPage<Error = ::sanakirja::Error> + ::sanakirja::RootPage> Send
    for GenericTxn<T>
{
}

impl Txn {
    pub fn check_database(&self, refs: &mut std::collections::BTreeMap<u64, usize>) {
        unsafe {
            use ::sanakirja::debug::Check;
            debug!("check: internal 0x{:x}", self.internal.db);
            self.internal.add_refs(&self.txn, refs).unwrap();
            debug!("check: external 0x{:x}", self.external.db);
            self.external.add_refs(&self.txn, refs).unwrap();
            debug!("check: inodes 0x{:x}", self.inodes.db);
            self.inodes.add_refs(&self.txn, refs).unwrap();
            debug!("check: revinodes 0x{:x}", self.revinodes.db);
            self.revinodes.add_refs(&self.txn, refs).unwrap();
            debug!("check: tree 0x{:x}", self.tree.db);
            self.tree.add_refs(&self.txn, refs).unwrap();
            debug!("check: revtree 0x{:x}", self.revtree.db);
            self.revtree.add_refs(&self.txn, refs).unwrap();
            debug!("check: revdep 0x{:x}", self.revdep.db);
            self.revdep.add_refs(&self.txn, refs).unwrap();
            debug!("check: dep 0x{:x}", self.dep.db);
            self.dep.add_refs(&self.txn, refs).unwrap();
            debug!("check: touched_files 0x{:x}", self.touched_files.db);
            self.touched_files.add_refs(&self.txn, refs).unwrap();
            debug!("check: rev_touched_files 0x{:x}", self.rev_touched_files.db);
            self.rev_touched_files.add_refs(&self.txn, refs).unwrap();
            debug!("check: partials 0x{:x}", self.partials.db);
            self.partials.add_refs(&self.txn, refs).unwrap();
            debug!("check: channels 0x{:x}", self.channels.db);
            self.channels.add_refs(&self.txn, refs).unwrap();
            for x in btree::iter(&self.txn, &self.channels, None).unwrap() {
                let (name, tup) = x.unwrap();
                debug!("check: channel name: {:?}", name.as_str());
                let graph: Db<Vertex<ChangeId>, SerializedEdge> = Db::from_page(tup.graph.into());
                let changes: Db<ChangeId, L64> = Db::from_page(tup.changes.into());
                let revchanges: UDb<L64, Pair<ChangeId, SerializedMerkle>> =
                    UDb::from_page(tup.revchanges.into());
                let states: UDb<SerializedMerkle, L64> = UDb::from_page(tup.states.into());
                let tags: Db<L64, Pair<SerializedMerkle, SerializedMerkle>> =
                    Db::from_page(tup.tags.into());
                debug!("check: graph 0x{:x}", graph.db);
                graph.add_refs(&self.txn, refs).unwrap();
                debug!("check: changes 0x{:x}", changes.db);
                changes.add_refs(&self.txn, refs).unwrap();
                debug!("check: revchanges 0x{:x}", revchanges.db);
                revchanges.add_refs(&self.txn, refs).unwrap();
                debug!("check: states 0x{:x}", states.db);
                states.add_refs(&self.txn, refs).unwrap();
                debug!("check: tags 0x{:x}", tags.db);
                tags.add_refs(&self.txn, refs).unwrap();
            }
            debug!("check: remotes 0x{:x}", self.remotes.db);
            self.remotes.add_refs(&self.txn, refs).unwrap();
            for x in btree::iter(&self.txn, &self.remotes, None).unwrap() {
                let (name, tup) = x.unwrap();
                debug!("check: remote name: {:?}", name);
                let remote: UDb<L64, Pair<SerializedHash, SerializedMerkle>> =
                    UDb::from_page(tup.remote.into());

                let rev: UDb<SerializedHash, L64> = UDb::from_page(tup.rev.into());
                let states: UDb<SerializedMerkle, L64> = UDb::from_page(tup.states.into());
                let tags: UDb<L64, Pair<SerializedMerkle, SerializedMerkle>> =
                    UDb::from_page(tup.tags.into());
                debug!("check: remote 0x{:x}", remote.db);
                remote.add_refs(&self.txn, refs).unwrap();
                debug!("check: rev 0x{:x}", rev.db);
                rev.add_refs(&self.txn, refs).unwrap();
                debug!("check: states 0x{:x}", states.db);
                states.add_refs(&self.txn, refs).unwrap();
                debug!("check: tags 0x{:x}", tags.db);
                tags.add_refs(&self.txn, refs).unwrap();
            }
            ::sanakirja::debug::add_free_refs(&self.txn, refs).unwrap();
            ::sanakirja::debug::check_free(&self.txn, refs);
        }
    }
}

impl<T: ::sanakirja::LoadPage<Error = ::sanakirja::Error> + ::sanakirja::RootPage> GraphTxnT
    for GenericTxn<T>
{
    type Graph = Channel;
    type GraphError = SanakirjaError;

    fn get_graph<'txn>(
        &'txn self,
        db: &Self::Graph,
        key: &Vertex<ChangeId>,
        value: Option<&SerializedEdge>,
    ) -> Result<Option<&'txn SerializedEdge>, TxnErr<Self::GraphError>> {
        match ::sanakirja::btree::get(&self.txn, &db.graph, key, value) {
            Ok(Some((k, v))) if k == key => Ok(Some(v)),
            Ok(_) => Ok(None),
            Err(e) => {
                error!("{:?}", e);
                Err(TxnErr(SanakirjaError::PristineCorrupt))
            }
        }
    }

    fn get_external(
        &self,
        p: &ChangeId,
    ) -> Result<&SerializedHash, GetExternalError<Self::GraphError>> {
        debug!("get_external {:?}", p);
        if p.is_root() {
            Ok(&HASH_NONE)
        } else {
            match btree::get(&self.txn, &self.external, p, None) {
                Ok(Some((k, v))) if k == p => Ok(v),
                Ok(_) => Err(GetExternalError::NotFound),
                Err(e) => {
                    error!("{:?}", e);
                    Err(GetExternalError::Txn(SanakirjaError::PristineCorrupt))
                }
            }
        }
    }

    fn get_internal(
        &self,
        p: &SerializedHash,
    ) -> Result<Option<&ChangeId>, TxnErr<Self::GraphError>> {
        if p.t == HashAlgorithm::None as u8 {
            Ok(Some(&ChangeId::ROOT))
        } else {
            match btree::get(&self.txn, &self.internal, p, None) {
                Ok(Some((k, v))) if k == p => Ok(Some(v)),
                Ok(_) => Ok(None),
                Err(e) => {
                    error!("{:?}", e);
                    Err(TxnErr(SanakirjaError::PristineCorrupt))
                }
            }
        }
    }

    type Adj = Adj;

    fn init_adj(
        &self,
        g: &Self::Graph,
        key: Vertex<ChangeId>,
        dest: Position<ChangeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<Self::Adj, TxnErr<Self::GraphError>> {
        let edge = SerializedEdge::new(min_flag, dest.change, dest.pos, ChangeId::ROOT);
        let mut cursor = btree::cursor::Cursor::new(&self.txn, &g.graph).map_err(TxnErr)?;
        cursor.set(&self.txn, &key, Some(&edge))?;
        Ok(Adj {
            cursor,
            key,
            min_flag,
            max_flag,
        })
    }

    fn next_adj<'a>(
        &'a self,
        _: &Self::Graph,
        a: &mut Self::Adj,
    ) -> Option<Result<&'a SerializedEdge, TxnErr<Self::GraphError>>> {
        next_adj(&self.txn, a).map(|x| x.map_err(|x| TxnErr(x.into())))
    }

    fn find_block(
        &self,
        graph: &Self::Graph,
        p: Position<ChangeId>,
    ) -> Result<&Vertex<ChangeId>, BlockError<Self::GraphError>> {
        Ok(find_block(&self.txn, &graph.graph, p)?)
    }

    fn find_block_end(
        &self,
        graph: &Self::Graph,
        p: Position<ChangeId>,
    ) -> Result<&Vertex<ChangeId>, BlockError<Self::GraphError>> {
        Ok(find_block_end(&self.txn, &graph.graph, p)?)
    }
}

impl std::convert::From<BlockError<::sanakirja::Error>> for BlockError<SanakirjaError> {
    fn from(e: BlockError<::sanakirja::Error>) -> Self {
        match e {
            BlockError::Txn(t) => BlockError::Txn(t.into()),
            BlockError::Block { block } => BlockError::Block { block },
        }
    }
}

#[doc(hidden)]
pub fn next_adj<'a, T>(txn: &'a T, a: &mut Adj) -> Option<Result<&'a SerializedEdge, T::Error>>
where
    T: sanakirja::LoadPage,
    T::Error: std::error::Error,
{
    loop {
        match a.cursor.next(txn).transpose()? {
            Err(error) => return Some(Err(error)),
            Ok((vertex, edge)) => {
                if *vertex == a.key && edge.flag() >= a.min_flag && edge.flag() <= a.max_flag {
                    return Some(Ok(edge));
                }
                if *vertex == a.key && edge.flag() >= a.min_flag && edge.flag() > a.max_flag {
                    return None;
                }
                if *vertex > a.key {
                    return None;
                }
            }
        }
    }
}

#[doc(hidden)]
pub fn find_block<'a, T: ::sanakirja::LoadPage>(
    txn: &'a T,
    graph: &::sanakirja::btree::Db<Vertex<ChangeId>, SerializedEdge>,
    p: Position<ChangeId>,
) -> Result<&'a Vertex<ChangeId>, BlockError<T::Error>>
where
    T::Error: std::error::Error,
{
    if p.change.is_root() {
        return Ok(&Vertex::ROOT);
    }
    let key = Vertex {
        change: p.change,
        start: p.pos,
        end: p.pos,
    };
    let mut cursor = btree::cursor::Cursor::new(txn, graph).map_err(BlockError::Txn)?;
    let mut k = if let Some((k, _)) = cursor.set(txn, &key, None).map_err(BlockError::Txn)? {
        k
    } else if let Some((k, _)) = cursor.prev(txn).map_err(BlockError::Txn)? {
        k
    } else {
        debug!("find_block: BLOCK ERROR");
        return Err(BlockError::Block { block: p });
    };
    // The only guarantee here is that k is either the first key >=
    // `key`. We might need to rewind by one step if key is strictly
    // larger than the result (i.e. if `p` is in the middle of the
    // key).
    while k.change > p.change || (k.change == p.change && k.start > p.pos) {
        if let Some((k_, _)) = cursor.prev(txn).map_err(BlockError::Txn)? {
            k = k_
        } else {
            break;
        }
    }
    loop {
        if k.change == p.change && k.start <= p.pos {
            if k.end > p.pos || (k.start == k.end && k.end == p.pos) {
                return Ok(k);
            }
        } else if k.change > p.change {
            debug!("find_block: BLOCK ERROR");
            return Err(BlockError::Block { block: p });
        }
        if let Some((k_, _)) = cursor.next(txn).map_err(BlockError::Txn)? {
            k = k_
        } else {
            break;
        }
    }
    debug!("find_block: BLOCK ERROR");
    Err(BlockError::Block { block: p })
}

#[doc(hidden)]
pub fn find_block_end<'a, T: ::sanakirja::LoadPage>(
    txn: &'a T,
    graph: &::sanakirja::btree::Db<Vertex<ChangeId>, SerializedEdge>,
    p: Position<ChangeId>,
) -> Result<&'a Vertex<ChangeId>, BlockError<T::Error>>
where
    T::Error: std::error::Error,
{
    if p.change.is_root() {
        return Ok(&Vertex::ROOT);
    }
    let key = Vertex {
        change: p.change,
        start: p.pos,
        end: p.pos,
    };
    let mut cursor = btree::cursor::Cursor::new(txn, graph).map_err(BlockError::Txn)?;
    debug!("key {:?}", key);
    let (mut k, v) = match cursor.set(txn, &key, None) {
        Ok(Some((k, v))) => (k, v),
        Ok(None) => {
            if let Some((k, v)) = cursor.prev(txn).map_err(BlockError::Txn)? {
                (k, v)
            } else {
                debug!("find_block_end, no prev");
                return Err(BlockError::Block { block: p });
            }
        }
        Err(e) => {
            debug!("find_block_end: BLOCK ERROR 0 {:?}", e);
            return Err(BlockError::Txn(e));
        }
    };
    debug!("cursor {:?} {:?}", k, v);
    loop {
        debug!("find_block_end, loop, k = {:?}, p = {:?}", k, p);
        if k.change < p.change {
            break;
        } else if k.change == p.change {
            // Here we want to create an edge pointing between `p`
            // and its successor. If k.start == p.pos, the only
            // case where that's what we want is if k.start ==
            // k.end.
            if k.start == p.pos && k.end == p.pos {
                return Ok(k);
            } else if k.start < p.pos {
                break;
            }
        }
        if let Some((k_, _)) = cursor.prev(txn).map_err(BlockError::Txn)? {
            k = k_
        } else {
            break;
        }
    }
    // We also want k.end >= p.pos, so we just call next() until
    // we have that.
    debug!("find_block_end, {:?} {:?}", k, p);
    while k.change < p.change || (k.change == p.change && p.pos > k.end) {
        if let Some((k_, _)) = cursor.next(txn).map_err(BlockError::Txn)? {
            k = k_
        } else {
            break;
        }
    }
    debug!("find_block_end, {:?} {:?}", k, p);
    if k.change == p.change
        && ((k.start < p.pos && p.pos <= k.end) || (k.start == k.end && k.start == p.pos))
    {
        Ok(k)
    } else {
        debug!("find_block_end: BLOCK ERROR");
        Err(BlockError::Block { block: p })
    }
}

pub struct Adj {
    pub cursor: ::sanakirja::btree::cursor::Cursor<
        Vertex<ChangeId>,
        SerializedEdge,
        P<Vertex<ChangeId>, SerializedEdge>,
    >,
    pub key: Vertex<ChangeId>,
    pub min_flag: EdgeFlags,
    pub max_flag: EdgeFlags,
}

impl<T: ::sanakirja::LoadPage<Error = ::sanakirja::Error> + ::sanakirja::RootPage> GraphIter
    for GenericTxn<T>
{
    type GraphCursor = ::sanakirja::btree::cursor::Cursor<
        Vertex<ChangeId>,
        SerializedEdge,
        P<Vertex<ChangeId>, SerializedEdge>,
    >;

    fn graph_cursor(
        &self,
        g: &Self::Graph,
        s: Option<&Vertex<ChangeId>>,
    ) -> Result<Self::GraphCursor, TxnErr<Self::GraphError>> {
        let mut c = ::sanakirja::btree::cursor::Cursor::new(&self.txn, &g.graph)?;
        if let Some(s) = s {
            c.set(&self.txn, s, None)?;
        }
        Ok(c)
    }

    fn next_graph<'txn>(
        &'txn self,
        _: &Self::Graph,
        a: &mut Self::GraphCursor,
    ) -> Option<Result<(&'txn Vertex<ChangeId>, &'txn SerializedEdge), TxnErr<Self::GraphError>>>
    {
        match a.next(&self.txn) {
            Ok(Some(x)) => Some(Ok(x)),
            Ok(None) => None,
            Err(e) => {
                error!("{:?}", e);
                Some(Err(TxnErr(SanakirjaError::PristineCorrupt)))
            }
        }
    }
}

// There is a choice here: the datastructure for `revchanges` is
// intuitively a list. Moreover, when removing a change, we must
// recompute the entire merkle tree after the removed change.
//
// This seems to indicate that a linked list could be an appropriate
// structure (a growable array is excluded because amortised
// complexity is not really acceptable here).
//
// However, we want to be able to answers queries such as "when was
// change X introduced?" without having to read the entire database.
//
// Additionally, even though `SerializedMerkle` has only one
// implementation, and is therefore sized in the current
// implementation, we can't exclude that other algorithms may be
// added, which means that the pages inside linked lists won't even be
// randomly-accessible arrays.
pub struct Channel {
    pub graph: Db<Vertex<ChangeId>, SerializedEdge>,
    pub changes: Db<ChangeId, L64>,
    pub revchanges: UDb<L64, Pair<ChangeId, SerializedMerkle>>,
    pub states: UDb<SerializedMerkle, L64>,
    pub tags: Db<L64, Pair<SerializedMerkle, SerializedMerkle>>,
    pub apply_counter: ApplyTimestamp,
    pub name: SmallString,
    pub last_modified: u64,
    pub id: RemoteId,
}

impl<T: ::sanakirja::LoadPage<Error = ::sanakirja::Error> + ::sanakirja::RootPage> ChannelTxnT
    for GenericTxn<T>
{
    type Channel = Channel;

    fn graph<'a>(&self, c: &'a Self::Channel) -> &'a Channel {
        c
    }
    fn name<'a>(&self, c: &'a Self::Channel) -> &'a str {
        c.name.as_str()
    }
    fn id<'a>(&self, c: &'a Self::Channel) -> Option<&'a RemoteId> {
        Some(&c.id)
    }
    fn apply_counter(&self, channel: &Self::Channel) -> u64 {
        channel.apply_counter
    }
    fn last_modified(&self, channel: &Self::Channel) -> u64 {
        channel.last_modified
    }
    fn changes<'a>(&self, channel: &'a Self::Channel) -> &'a Self::Changeset {
        &channel.changes
    }
    fn rev_changes<'a>(&self, channel: &'a Self::Channel) -> &'a Self::RevChangeset {
        &channel.revchanges
    }
    fn tags<'a>(&self, channel: &'a Self::Channel) -> &'a Self::Tags {
        &channel.tags
    }

    type Changeset = Db<ChangeId, L64>;
    type RevChangeset = UDb<L64, Pair<ChangeId, SerializedMerkle>>;

    fn get_changeset(
        &self,
        channel: &Self::Changeset,
        c: &ChangeId,
    ) -> Result<Option<&L64>, TxnErr<Self::GraphError>> {
        match btree::get(&self.txn, channel, c, None) {
            Ok(Some((k, x))) if k == c => Ok(Some(x)),
            Ok(x) => {
                debug!("get_changeset = {:?}", x);
                Ok(None)
            }
            Err(e) => {
                error!("{:?}", e);
                Err(TxnErr(SanakirjaError::PristineCorrupt))
            }
        }
    }
    fn get_revchangeset(
        &self,
        revchanges: &Self::RevChangeset,
        c: &L64,
    ) -> Result<Option<&Pair<ChangeId, SerializedMerkle>>, TxnErr<Self::GraphError>> {
        match btree::get(&self.txn, revchanges, c, None) {
            Ok(Some((k, x))) if k == c => Ok(Some(x)),
            Ok(_) => Ok(None),
            Err(e) => {
                error!("{:?}", e);
                Err(TxnErr(SanakirjaError::PristineCorrupt))
            }
        }
    }

    type ChangesetCursor = ::sanakirja::btree::cursor::Cursor<ChangeId, L64, P<ChangeId, L64>>;

    fn cursor_changeset<'a>(
        &'a self,
        channel: &Self::Changeset,
        pos: Option<ChangeId>,
    ) -> Result<Cursor<Self, &'a Self, Self::ChangesetCursor, ChangeId, L64>, TxnErr<SanakirjaError>>
    {
        let mut cursor = btree::cursor::Cursor::new(&self.txn, channel)?;
        if let Some(k) = pos {
            cursor.set(&self.txn, &k, None)?;
        }
        Ok(Cursor {
            cursor,
            txn: self,
            k: std::marker::PhantomData,
            v: std::marker::PhantomData,
            t: std::marker::PhantomData,
        })
    }

    type RevchangesetCursor = ::sanakirja::btree::cursor::Cursor<
        L64,
        Pair<ChangeId, SerializedMerkle>,
        UP<L64, Pair<ChangeId, SerializedMerkle>>,
    >;

    fn cursor_revchangeset_ref<'a, RT: std::ops::Deref<Target = Self>>(
        txn: RT,
        channel: &Self::RevChangeset,
        pos: Option<L64>,
    ) -> Result<
        Cursor<Self, RT, Self::RevchangesetCursor, L64, Pair<ChangeId, SerializedMerkle>>,
        TxnErr<SanakirjaError>,
    > {
        let mut cursor = btree::cursor::Cursor::new(&txn.txn, channel)?;
        if let Some(k) = pos {
            cursor.set(&txn.txn, &k, None)?;
        }
        Ok(Cursor {
            cursor,
            txn,
            k: std::marker::PhantomData,
            v: std::marker::PhantomData,
            t: std::marker::PhantomData,
        })
    }

    fn rev_cursor_revchangeset<'a>(
        &'a self,
        channel: &Self::RevChangeset,
        pos: Option<L64>,
    ) -> Result<
        RevCursor<Self, &'a Self, Self::RevchangesetCursor, L64, Pair<ChangeId, SerializedMerkle>>,
        TxnErr<SanakirjaError>,
    > {
        let mut cursor = btree::cursor::Cursor::new(&self.txn, channel)?;
        if let Some(ref pos) = pos {
            cursor.set(&self.txn, pos, None)?;
        } else {
            cursor.set_last(&self.txn)?;
        };
        Ok(RevCursor {
            cursor,
            txn: self,
            k: std::marker::PhantomData,
            v: std::marker::PhantomData,
            t: std::marker::PhantomData,
        })
    }

    fn cursor_revchangeset_next(
        &self,
        cursor: &mut Self::RevchangesetCursor,
    ) -> Result<Option<(&L64, &Pair<ChangeId, SerializedMerkle>)>, TxnErr<SanakirjaError>> {
        if let Ok(x) = cursor.next(&self.txn) {
            Ok(x)
        } else {
            Err(TxnErr(SanakirjaError::PristineCorrupt))
        }
    }
    fn cursor_revchangeset_prev(
        &self,
        cursor: &mut Self::RevchangesetCursor,
    ) -> Result<Option<(&L64, &Pair<ChangeId, SerializedMerkle>)>, TxnErr<SanakirjaError>> {
        if let Ok(x) = cursor.prev(&self.txn) {
            Ok(x)
        } else {
            Err(TxnErr(SanakirjaError::PristineCorrupt))
        }
    }

    fn cursor_changeset_next(
        &self,
        cursor: &mut Self::ChangesetCursor,
    ) -> Result<Option<(&ChangeId, &L64)>, TxnErr<SanakirjaError>> {
        if let Ok(x) = cursor.next(&self.txn) {
            Ok(x)
        } else {
            Err(TxnErr(SanakirjaError::PristineCorrupt))
        }
    }
    fn cursor_changeset_prev(
        &self,
        cursor: &mut Self::ChangesetCursor,
    ) -> Result<Option<(&ChangeId, &L64)>, TxnErr<SanakirjaError>> {
        if let Ok(x) = cursor.prev(&self.txn) {
            Ok(x)
        } else {
            Err(TxnErr(SanakirjaError::PristineCorrupt))
        }
    }

    type States = UDb<SerializedMerkle, L64>;
    fn states<'a>(&self, channel: &'a Self::Channel) -> &'a Self::States {
        &channel.states
    }
    fn channel_has_state(
        &self,
        channel: &Self::States,
        m: &SerializedMerkle,
    ) -> Result<Option<L64>, TxnErr<Self::GraphError>> {
        match btree::get(&self.txn, channel, m, None)? {
            Some((k, v)) if k == m => Ok(Some(*v)),
            _ => Ok(None),
        }
    }

    type Tags = Db<L64, Pair<SerializedMerkle, SerializedMerkle>>;

    fn is_tagged(&self, tags: &Self::Tags, t: u64) -> Result<bool, TxnErr<Self::GraphError>> {
        let t: L64 = t.into();
        match btree::get(&self.txn, tags, &t, None)? {
            Some((k, _)) => Ok(k == &t),
            _ => Ok(false),
        }
    }

    type TagsCursor = ::sanakirja::btree::cursor::Cursor<
        L64,
        Pair<SerializedMerkle, SerializedMerkle>,
        P<L64, Pair<SerializedMerkle, SerializedMerkle>>,
    >;
    fn cursor_tags<'txn>(
        &'txn self,
        channel: &Self::Tags,
        k: Option<L64>,
    ) -> Result<
        crate::pristine::Cursor<
            Self,
            &'txn Self,
            Self::TagsCursor,
            L64,
            Pair<SerializedMerkle, SerializedMerkle>,
        >,
        TxnErr<Self::GraphError>,
    > {
        let mut cursor = btree::cursor::Cursor::new(&self.txn, channel)?;
        if let Some(k) = k {
            cursor.set(&self.txn, &k, None)?;
        }
        Ok(Cursor {
            cursor,
            txn: self,
            k: std::marker::PhantomData,
            v: std::marker::PhantomData,
            t: std::marker::PhantomData,
        })
    }
    fn cursor_tags_next(
        &self,
        cursor: &mut Self::TagsCursor,
    ) -> Result<Option<(&L64, &Pair<SerializedMerkle, SerializedMerkle>)>, TxnErr<Self::GraphError>>
    {
        if let Ok(x) = cursor.next(&self.txn) {
            Ok(x)
        } else {
            Err(TxnErr(SanakirjaError::PristineCorrupt))
        }
    }

    fn cursor_tags_prev(
        &self,
        cursor: &mut Self::TagsCursor,
    ) -> Result<Option<(&L64, &Pair<SerializedMerkle, SerializedMerkle>)>, TxnErr<Self::GraphError>>
    {
        if let Ok(x) = cursor.prev(&self.txn) {
            Ok(x)
        } else {
            Err(TxnErr(SanakirjaError::PristineCorrupt))
        }
    }

    fn iter_tags(
        &self,
        channel: &Self::Tags,
        from: u64,
    ) -> Result<
        super::Cursor<Self, &Self, Self::TagsCursor, L64, Pair<SerializedMerkle, SerializedMerkle>>,
        TxnErr<Self::GraphError>,
    > {
        self.cursor_tags(channel, Some(from.into()))
    }

    fn rev_iter_tags(
        &self,
        channel: &Self::Tags,
        from: Option<u64>,
    ) -> Result<
        super::RevCursor<
            Self,
            &Self,
            Self::TagsCursor,
            L64,
            Pair<SerializedMerkle, SerializedMerkle>,
        >,
        TxnErr<Self::GraphError>,
    > {
        let mut cursor = btree::cursor::Cursor::new(&self.txn, channel)?;
        if let Some(from) = from {
            cursor.set(&self.txn, &from.into(), None)?;
        } else {
            cursor.set_last(&self.txn)?;
        };
        Ok(RevCursor {
            cursor,
            txn: self,
            k: std::marker::PhantomData,
            v: std::marker::PhantomData,
            t: std::marker::PhantomData,
        })
    }
}

impl<T: ::sanakirja::LoadPage<Error = ::sanakirja::Error> + ::sanakirja::RootPage> DepsTxnT
    for GenericTxn<T>
{
    type DepsError = SanakirjaError;
    type Dep = Db<ChangeId, ChangeId>;
    type Revdep = Db<ChangeId, ChangeId>;

    sanakirja_table_get!(dep, ChangeId, ChangeId, DepsError);
    sanakirja_table_get!(revdep, ChangeId, ChangeId, DepsError);
    type DepCursor = ::sanakirja::btree::cursor::Cursor<ChangeId, ChangeId, P<ChangeId, ChangeId>>;
    sanakirja_cursor_ref!(dep, ChangeId, ChangeId);
    fn iter_dep_ref<RT: std::ops::Deref<Target = Self> + Clone>(
        txn: RT,
        p: &ChangeId,
    ) -> Result<super::Cursor<Self, RT, Self::DepCursor, ChangeId, ChangeId>, TxnErr<Self::DepsError>>
    {
        Self::cursor_dep_ref(txn.clone(), &txn.dep, Some((p, None)))
    }

    sanakirja_table_get!(touched_files, Position<ChangeId>, ChangeId, DepsError);
    sanakirja_table_get!(rev_touched_files, ChangeId, Position<ChangeId>, DepsError);

    type Touched_files = Db<Position<ChangeId>, ChangeId>;

    type Rev_touched_files = Db<ChangeId, Position<ChangeId>>;

    type Touched_filesCursor = ::sanakirja::btree::cursor::Cursor<
        Position<ChangeId>,
        ChangeId,
        P<Position<ChangeId>, ChangeId>,
    >;
    sanakirja_iter!(touched_files, Position<ChangeId>, ChangeId);

    type Rev_touched_filesCursor = ::sanakirja::btree::cursor::Cursor<
        ChangeId,
        Position<ChangeId>,
        P<ChangeId, Position<ChangeId>>,
    >;
    sanakirja_iter!(rev_touched_files, ChangeId, Position<ChangeId>);
    fn iter_revdep(
        &self,
        k: &ChangeId,
    ) -> Result<
        super::Cursor<Self, &Self, Self::DepCursor, ChangeId, ChangeId>,
        TxnErr<Self::DepsError>,
    > {
        self.cursor_dep(&self.revdep, Some((k, None)))
    }

    fn iter_dep(
        &self,
        k: &ChangeId,
    ) -> Result<
        super::Cursor<Self, &Self, Self::DepCursor, ChangeId, ChangeId>,
        TxnErr<Self::DepsError>,
    > {
        self.cursor_dep(&self.dep, Some((k, None)))
    }

    fn iter_touched(
        &self,
        k: &Position<ChangeId>,
    ) -> Result<
        super::Cursor<Self, &Self, Self::Touched_filesCursor, Position<ChangeId>, ChangeId>,
        TxnErr<Self::DepsError>,
    > {
        self.cursor_touched_files(&self.touched_files, Some((k, None)))
    }

    fn iter_rev_touched(
        &self,
        k: &ChangeId,
    ) -> Result<
        super::Cursor<Self, &Self, Self::Rev_touched_filesCursor, ChangeId, Position<ChangeId>>,
        TxnErr<Self::DepsError>,
    > {
        self.cursor_rev_touched_files(&self.rev_touched_files, Some((k, None)))
    }
}

impl<T: ::sanakirja::LoadPage<Error = ::sanakirja::Error> + ::sanakirja::RootPage> TreeTxnT
    for GenericTxn<T>
{
    type TreeError = SanakirjaError;
    type Inodes = Db<Inode, SerializedInode>;
    type Revinodes = Db<Position<ChangeId>, Inode>;
    // Hand-written (the `inodes` value is `SerializedInode` since format v2, but
    // callers still want the graph `Position`): return a reference to the
    // `position` field of the stored record.
    fn get_inodes<'txn>(
        &'txn self,
        key: &Inode,
        _value: Option<&Position<ChangeId>>,
    ) -> Result<Option<&'txn Position<ChangeId>>, TreeErr<Self::TreeError>> {
        match ::sanakirja::btree::get(&self.txn, &self.inodes, key, None) {
            Ok(Some((k, v))) if k == key => Ok(Some(&v.position)),
            Ok(_) => Ok(None),
            Err(e) => {
                error!("{:?}", e);
                Err(TreeErr(SanakirjaError::PristineCorrupt))
            }
        }
    }
    fn get_inode_stat(&self, key: &Inode) -> Result<Option<(u64, u64)>, TreeErr<Self::TreeError>> {
        match ::sanakirja::btree::get(&self.txn, &self.inodes, key, None) {
            Ok(Some((k, v))) if k == key => {
                if v.mtime.as_u64() == 0 {
                    Ok(None)
                } else {
                    Ok(Some((v.mtime.as_u64(), v.size.as_u64())))
                }
            }
            Ok(_) => Ok(None),
            Err(e) => {
                error!("{:?}", e);
                Err(TreeErr(SanakirjaError::PristineCorrupt))
            }
        }
    }
    sanakirja_table_get!(revinodes, Position<ChangeId>, Inode, TreeError, TreeErr);
    sanakirja_cursor!(inodes, Inode, SerializedInode, TreeErr);
    // #[cfg(debug_assertions)]
    sanakirja_cursor!(revinodes, Position<ChangeId>, Inode, TreeErr);

    type Tree = UDb<PathId, Inode>;
    sanakirja_table_get!(tree, PathId, Inode, TreeError, TreeErr);
    type TreeCursor = ::sanakirja::btree::cursor::Cursor<PathId, Inode, UP<PathId, Inode>>;
    sanakirja_iter!(tree, PathId, Inode, TreeErr);
    type RevtreeCursor = ::sanakirja::btree::cursor::Cursor<Inode, PathId, UP<Inode, PathId>>;
    sanakirja_iter!(revtree, Inode, PathId, TreeErr);

    type Revtree = UDb<Inode, PathId>;
    sanakirja_table_get!(revtree, Inode, PathId, TreeError, TreeErr);

    type Partials = UDb<SmallStr, Position<ChangeId>>;
    type PartialsCursor = ::sanakirja::btree::cursor::Cursor<
        SmallStr,
        Position<ChangeId>,
        UP<SmallStr, Position<ChangeId>>,
    >;
    sanakirja_cursor!(partials, SmallStr, Position<ChangeId>, TreeErr);
    type InodesCursor =
        ::sanakirja::btree::cursor::Cursor<Inode, SerializedInode, P<Inode, SerializedInode>>;
    fn iter_inodes(
        &self,
    ) -> Result<
        super::Cursor<Self, &Self, Self::InodesCursor, Inode, SerializedInode>,
        TreeErr<Self::TreeError>,
    > {
        self.cursor_inodes(&self.inodes, None)
    }

    // #[cfg(debug_assertions)]
    type RevinodesCursor =
        ::sanakirja::btree::cursor::Cursor<Position<ChangeId>, Inode, P<Position<ChangeId>, Inode>>;
    // #[cfg(debug_assertions)]
    fn iter_revinodes(
        &self,
    ) -> Result<
        super::Cursor<Self, &Self, Self::RevinodesCursor, Position<ChangeId>, Inode>,
        TreeErr<SanakirjaError>,
    > {
        self.cursor_revinodes(&self.revinodes, None)
    }

    fn iter_partials<'txn>(
        &'txn self,
        k0: &crate::small_string::SmallStr,
    ) -> Result<
        super::Cursor<Self, &'txn Self, Self::PartialsCursor, SmallStr, Position<ChangeId>>,
        TreeErr<SanakirjaError>,
    > {
        self.cursor_partials(&self.partials, Some((k0, None)))
    }
}

impl<T: ::sanakirja::LoadPage<Error = ::sanakirja::Error> + ::sanakirja::RootPage> GenericTxn<T> {
    #[doc(hidden)]
    pub unsafe fn unsafe_load_channel(
        &self,
        name: SmallString,
    ) -> Result<Option<Channel>, TxnErr<SanakirjaError>> {
        unsafe {
            debug!("unsafe load channel");
            match btree::get(&self.txn, &self.channels, &name, None)? {
                Some((name_, tup)) if name_ == name.as_ref() => {
                    debug!("load_channel: {:?} {:?}", name, tup);
                    Ok(Some(Channel {
                        graph: Db::from_page(tup.graph.into()),
                        changes: Db::from_page(tup.changes.into()),
                        revchanges: UDb::from_page(tup.revchanges.into()),
                        states: UDb::from_page(tup.states.into()),
                        tags: Db::from_page(tup.tags.into()),
                        apply_counter: tup.apply_counter.into(),
                        last_modified: tup.last_modified.into(),
                        id: tup.id,
                        name,
                    }))
                }
                _ => {
                    debug!("unsafe_load_channel: not found");
                    Ok(None)
                }
            }
        }
    }
}

impl<T: ::sanakirja::LoadPage<Error = ::sanakirja::Error> + ::sanakirja::RootPage> TxnT
    for GenericTxn<T>
{
    fn is_superseded(&self, h: &Hash) -> Result<bool, TxnErr<Self::GraphError>> {
        // Obsolescence markers live in the `superseded` table (opened in
        // `*_txn_begin`, absent on a pristine predating the feature — then
        // nothing is superseded until the first amend creates it).
        let db = match self.superseded {
            Some(ref db) => db,
            None => return Ok(false),
        };
        let k: SerializedHash = h.into();
        match btree::get(&self.txn, db, &k, None) {
            Ok(Some((found, _))) if found == &k => Ok(true),
            Ok(_) => Ok(false),
            Err(e) => {
                error!("{:?}", e);
                Err(TxnErr(SanakirjaError::PristineCorrupt))
            }
        }
    }

    fn hash_from_prefix(
        &self,
        s: &str,
    ) -> Result<(Hash, ChangeId), super::HashPrefixError<Self::GraphError>> {
        let mut result = None;
        for h in prefix_guesses(s) {
            let h: SerializedHash = (&h).into();
            debug!("h = {:?}", h);
            for x in btree::iter(&self.txn, &self.internal, Some((&h, None)))
                .map_err(|e| super::HashPrefixError::Txn(e.into()))?
            {
                let (e, i) = x.map_err(|e| super::HashPrefixError::Txn(e.into()))?;
                debug!("{:?}", e);
                if e < &h {
                    continue;
                } else {
                    let e: Hash = e.into();
                    let b32 = e.to_base32();
                    debug!("{:?}", b32);
                    let (b32, _) = b32.split_at(s.len().min(b32.len()));
                    if b32 != s {
                        break;
                    } else if result.is_none() {
                        result = Some((e, *i))
                    } else {
                        return Err(super::HashPrefixError::Ambiguous(s.to_string()));
                    }
                }
            }
        }
        if let Some(result) = result {
            return Ok(result);
        }
        Err(super::HashPrefixError::NotFound(s.to_string()))
    }

    fn state_from_prefix(
        &self,
        channel: &Self::States,
        s: &str,
    ) -> Result<(Merkle, L64), super::HashPrefixError<Self::GraphError>> {
        let h = if let Some(h) = SerializedMerkle::from_prefix(s) {
            h
        } else {
            return Err(super::HashPrefixError::Parse(s.to_string()));
        };
        let mut result = None;
        debug!("h = {:?}", h);
        for x in btree::iter(&self.txn, channel, Some((&h, None)))
            .map_err(|e| super::HashPrefixError::Txn(e.into()))?
        {
            let (e, i) = x.map_err(|e| super::HashPrefixError::Txn(e.into()))?;
            debug!("{:?} {:?}", e, i);
            if e < &h {
                continue;
            } else {
                let e: Merkle = e.into();
                let b32 = e.to_base32();
                debug!("{:?}", b32);
                let (b32, _) = b32.split_at(s.len().min(b32.len()));
                if b32 != s {
                    break;
                } else if result.is_none() {
                    result = Some((e, *i))
                } else {
                    return Err(super::HashPrefixError::Ambiguous(s.to_string()));
                }
            }
        }
        if let Some(result) = result {
            Ok(result)
        } else {
            Err(super::HashPrefixError::NotFound(s.to_string()))
        }
    }

    fn hash_from_prefix_remote(
        &self,
        remote: &RemoteRef<Self>,
        s: &str,
    ) -> Result<Hash, super::HashPrefixError<Self::GraphError>> {
        let remote = remote.db.lock();

        let mut result = None;

        for h in prefix_guesses(s) {
            let h: SerializedHash = (&h).into();
            debug!("h = {:?}", h);
            for x in btree::iter(&self.txn, &remote.rev, Some((&h, None)))
                .map_err(|e| super::HashPrefixError::Txn(e.into()))?
            {
                let (e, _) = x.map_err(|e| super::HashPrefixError::Txn(e.into()))?;
                debug!("{:?}", e);
                if e < &h {
                    continue;
                } else {
                    let e: Hash = e.into();
                    let b32 = e.to_base32();
                    debug!("{:?}", b32);
                    let (b32, _) = b32.split_at(s.len().min(b32.len()));
                    if b32 != s {
                        break;
                    } else if result.is_none() {
                        result = Some(e)
                    } else {
                        return Err(super::HashPrefixError::Ambiguous(s.to_string()));
                    }
                }
            }
        }
        if let Some(result) = result {
            return Ok(result);
        }
        Err(super::HashPrefixError::NotFound(s.to_string()))
    }

    fn load_channel(
        &self,
        name: SmallString,
    ) -> Result<Option<ChannelRef<Self>>, TxnErr<Self::GraphError>> {
        match self.open_channels.lock().entry(name.clone()) {
            Entry::Vacant(v) => {
                if let Some(c) = unsafe { self.unsafe_load_channel(name)? } {
                    Ok(Some(
                        v.insert(ChannelRef {
                            r: Arc::new(RwLock::new(c)),
                        })
                        .clone(),
                    ))
                } else {
                    Ok(None)
                }
            }
            Entry::Occupied(occ) => Ok(Some(occ.get().clone())),
        }
    }

    fn load_remote(
        &self,
        name: &RemoteId,
    ) -> Result<Option<RemoteRef<Self>>, TxnErr<Self::GraphError>> {
        unsafe {
            let name = name.to_owned();
            match self.open_remotes.lock().entry(name) {
                Entry::Vacant(v) => match btree::get(&self.txn, &self.remotes, &name, None)? {
                    Some((name_, remote)) if name == *name_ => {
                        debug!("load_remote: {:?} {:?}", name_, remote);
                        let r = Remote {
                            remote: UDb::from_page(remote.remote.into()),
                            rev: UDb::from_page(remote.rev.into()),
                            states: UDb::from_page(remote.states.into()),
                            id_rev: remote.id_rev,
                            tags: Db::from_page(remote.tags.into()),
                            path: remote.path.to_owned(),
                        };
                        for x in btree::iter(&self.txn, &r.remote, None).unwrap() {
                            debug!("remote -> {:?}", x);
                        }
                        for x in btree::iter(&self.txn, &r.rev, None).unwrap() {
                            debug!("rev -> {:?}", x);
                        }
                        for x in btree::iter(&self.txn, &r.states, None).unwrap() {
                            debug!("states -> {:?}", x);
                        }

                        for x in self.iter_remote(&r.remote, 0).unwrap() {
                            debug!("ITER {:?}", x);
                        }

                        let r = RemoteRef {
                            db: Arc::new(Mutex::new(r)),
                            id: name,
                        };
                        Ok(Some(v.insert(r).clone()))
                    }
                    _ => Ok(None),
                },
                Entry::Occupied(occ) => Ok(Some(occ.get().clone())),
            }
        }
    }

    type Channels = UDb<SmallStr, SerializedChannel>;
    type ChannelsCursor = ::sanakirja::btree::cursor::Cursor<
        SmallStr,
        SerializedChannel,
        UP<SmallStr, SerializedChannel>,
    >;
    sanakirja_cursor!(channels, SmallStr, SerializedChannel);
    fn channels(
        &self,
        start: &SmallStr,
    ) -> Result<Vec<ChannelRef<Self>>, TxnErr<Self::GraphError>> {
        let name = start;
        let mut cursor = btree::cursor::Cursor::new(&self.txn, &self.channels)?;
        cursor.set(&self.txn, name, None)?;
        while let Ok(Some((name, _))) = self.cursor_channels_next(&mut cursor) {
            self.load_channel(name.to_owned())?;
        }
        Ok(self.open_channels.lock().values().cloned().collect())
    }

    type Remotes = UDb<RemoteId, SerializedRemote>;
    type RemotesCursor = ::sanakirja::btree::cursor::Cursor<
        RemoteId,
        SerializedRemote,
        UP<RemoteId, SerializedRemote>,
    >;
    sanakirja_cursor!(remotes, RemoteId, SerializedRemote);
    fn iter_remotes<'txn>(
        &'txn self,
        start: &RemoteId,
    ) -> Result<RemotesIterator<'txn, Self>, TxnErr<Self::GraphError>> {
        let mut cursor = btree::cursor::Cursor::new(&self.txn, &self.remotes)?;
        cursor.set(&self.txn, start, None)?;
        Ok(RemotesIterator { cursor, txn: self })
    }

    type Remote = UDb<L64, Pair<SerializedHash, SerializedMerkle>>;
    type Revremote = UDb<SerializedHash, L64>;
    type Remotestates = UDb<SerializedMerkle, L64>;
    type Remotetags = UDb<L64, Pair<SerializedMerkle, SerializedMerkle>>;
    type RemoteCursor = ::sanakirja::btree::cursor::Cursor<
        L64,
        Pair<SerializedHash, SerializedMerkle>,
        UP<L64, Pair<SerializedHash, SerializedMerkle>>,
    >;
    sanakirja_cursor!(remote, L64, Pair<SerializedHash, SerializedMerkle>);
    sanakirja_rev_cursor!(remote, L64, Pair<SerializedHash, SerializedMerkle>);

    fn iter_remote<'txn>(
        &'txn self,
        remote: &Self::Remote,
        k: u64,
    ) -> Result<
        super::Cursor<
            Self,
            &'txn Self,
            Self::RemoteCursor,
            L64,
            Pair<SerializedHash, SerializedMerkle>,
        >,
        TxnErr<Self::GraphError>,
    > {
        self.cursor_remote(remote, Some((&k.into(), None)))
    }

    fn iter_rev_remote<'txn>(
        &'txn self,
        remote: &Self::Remote,
        k: Option<L64>,
    ) -> Result<
        super::RevCursor<
            Self,
            &'txn Self,
            Self::RemoteCursor,
            L64,
            Pair<SerializedHash, SerializedMerkle>,
        >,
        TxnErr<Self::GraphError>,
    > {
        self.rev_cursor_remote(remote, k.as_ref().map(|k| (k, None)))
    }

    fn get_remote(
        &mut self,
        name: RemoteId,
    ) -> Result<Option<RemoteRef<Self>>, TxnErr<Self::GraphError>> {
        unsafe {
            let name = name.to_owned();
            match self.open_remotes.lock().entry(name) {
                Entry::Vacant(v) => match btree::get(&self.txn, &self.remotes, &name, None)? {
                    Some((name_, remote)) if *name_ == name => {
                        let r = RemoteRef {
                            db: Arc::new(Mutex::new(Remote {
                                remote: UDb::from_page(remote.remote.into()),
                                rev: UDb::from_page(remote.rev.into()),
                                states: UDb::from_page(remote.states.into()),
                                id_rev: remote.id_rev,
                                tags: Db::from_page(remote.tags.into()),
                                path: remote.path.to_owned(),
                            })),
                            id: name,
                        };
                        v.insert(r);
                    }
                    _ => return Ok(None),
                },
                Entry::Occupied(_) => {}
            }
            Ok(self.open_remotes.lock().get(&name).cloned())
        }
    }

    fn last_remote(
        &self,
        remote: &Self::Remote,
    ) -> Result<Option<(u64, &Pair<SerializedHash, SerializedMerkle>)>, TxnErr<Self::GraphError>>
    {
        debug!("last_remote: {:?}", remote);
        if let Some(x) = btree::rev_iter(&self.txn, remote, None)?.next() {
            let (&k, v) = x?;
            Ok(Some((k.into(), v)))
        } else {
            Ok(None)
        }
    }

    fn last_remote_tag(
        &self,
        remote: &Self::Tags,
    ) -> Result<Option<(u64, &SerializedMerkle, &SerializedMerkle)>, TxnErr<Self::GraphError>> {
        if let Some(x) = btree::rev_iter(&self.txn, remote, None)?.next() {
            let (&k, v) = x?;
            Ok(Some((k.into(), &v.a, &v.b)))
        } else {
            Ok(None)
        }
    }

    fn get_remote_state(
        &self,
        remote: &Self::Remote,
        n: u64,
    ) -> Result<Option<(u64, &Pair<SerializedHash, SerializedMerkle>)>, TxnErr<Self::GraphError>>
    {
        let n = n.into();
        for x in btree::iter(&self.txn, remote, Some((&n, None)))? {
            let (&k, m) = x?;
            if k >= n {
                return Ok(Some((k.into(), m)));
            }
        }
        Ok(None)
    }

    fn get_remote_tag(
        &self,
        remote: &Self::Tags,
        n: u64,
    ) -> Result<Option<(u64, &Pair<SerializedMerkle, SerializedMerkle>)>, TxnErr<Self::GraphError>>
    {
        let n = n.into();
        if let Some(x) = btree::rev_iter(&self.txn, remote, Some((&n, None)))?.next() {
            let (&k, m) = x?;
            Ok(Some((k.into(), m)))
        } else {
            Ok(None)
        }
    }

    fn remote_has_change(
        &self,
        remote: &RemoteRef<Self>,
        hash: &SerializedHash,
    ) -> Result<bool, TxnErr<Self::GraphError>> {
        match btree::get(&self.txn, &remote.db.lock().rev, hash, None)? {
            Some((k, _)) if k == hash => Ok(true),
            _ => Ok(false),
        }
    }
    fn remote_has_state(
        &self,
        remote: &RemoteRef<Self>,
        m: &SerializedMerkle,
    ) -> Result<Option<u64>, TxnErr<Self::GraphError>> {
        match btree::get(&self.txn, &remote.db.lock().states, m, None)? {
            Some((k, v)) if k == m => Ok(Some((*v).into())),
            _ => Ok(None),
        }
    }
    fn current_channel(&self) -> Result<&str, Self::GraphError> {
        if let Some(ref c) = self.cur_channel {
            Ok(c)
        } else {
            unsafe {
                let b = self.txn.root_page();
                let len = b[4096 - 256] as usize;
                if len == 0 {
                    Ok("main")
                } else {
                    let s = std::slice::from_raw_parts(b.as_ptr().add(4096 - 255), len);
                    Ok(std::str::from_utf8(s).unwrap_or("main"))
                }
            }
        }
    }
}

impl<
    T: sanakirja::AllocPage<Error = ::sanakirja::Error>
        + sanakirja::RootPage
        + sanakirja::LoadPage<Error = ::sanakirja::Error>,
> GraphMutTxnT for MutTxn<T>
{
    fn put_graph(
        &mut self,
        graph: &mut Self::Graph,
        k: &Vertex<ChangeId>,
        e: &SerializedEdge,
    ) -> Result<bool, TxnErr<Self::GraphError>> {
        Ok(btree::put(&mut self.txn, &mut graph.graph, k, e)?)
    }

    fn del_graph(
        &mut self,
        graph: &mut Self::Graph,
        k: &Vertex<ChangeId>,
        e: Option<&SerializedEdge>,
    ) -> Result<bool, TxnErr<Self::GraphError>> {
        Ok(btree::del(&mut self.txn, &mut graph.graph, k, e)?)
    }

    fn debug(&mut self, graph: &mut Self::Graph, extra: &str) {
        ::sanakirja::debug::debug(
            &self.txn,
            &[&graph.graph],
            format!("debug{}{}", self.counter, extra),
            true,
        );
    }

    sanakirja_put_del!(internal, SerializedHash, ChangeId, GraphError);
    sanakirja_put_del!(external, ChangeId, SerializedHash, GraphError);

    fn split_block(
        &mut self,
        graph: &mut Self::Graph,
        key: &Vertex<ChangeId>,
        pos: ChangePosition,
        buf: &mut Vec<SerializedEdge>,
    ) -> Result<(), TxnErr<Self::GraphError>> {
        assert!(pos > key.start);
        assert!(pos < key.end);
        let mut cursor = btree::cursor::Cursor::new(&self.txn, &graph.graph)?;
        cursor.set(&self.txn, key, None)?;
        loop {
            match cursor.next(&self.txn) {
                Ok(Some((k, v))) => {
                    if k > key {
                        break;
                    } else if k < key {
                        continue;
                    }
                    buf.push(*v)
                }
                Ok(None) => break,
                Err(e) => {
                    error!("{:?}", e);
                    return Err(TxnErr(SanakirjaError::PristineCorrupt));
                }
            }
        }
        for chi in buf.drain(..) {
            assert!(
                chi.introduced_by() != ChangeId::ROOT || chi.flag().contains(EdgeFlags::PSEUDO)
            );
            if chi.flag().contains(EdgeFlags::PARENT | EdgeFlags::BLOCK) {
                put_graph_with_rev(
                    self,
                    graph,
                    chi.flag() - EdgeFlags::PARENT,
                    Vertex {
                        change: key.change,
                        start: key.start,
                        end: pos,
                    },
                    Vertex {
                        change: key.change,
                        start: pos,
                        end: key.end,
                    },
                    chi.introduced_by(),
                )?;
            }

            self.del_graph(graph, key, Some(&chi))?;
            self.put_graph(
                graph,
                &if chi.flag().contains(EdgeFlags::PARENT) {
                    Vertex {
                        change: key.change,
                        start: key.start,
                        end: pos,
                    }
                } else {
                    Vertex {
                        change: key.change,
                        start: pos,
                        end: key.end,
                    }
                },
                &chi,
            )?;
        }
        Ok(())
    }
}

impl<
    T: sanakirja::AllocPage<Error = ::sanakirja::Error>
        + sanakirja::RootPage
        + sanakirja::LoadPage<Error = ::sanakirja::Error>,
> ChannelMutTxnT for MutTxn<T>
{
    fn graph_mut(c: &mut Self::Channel) -> &mut Self::Graph {
        c
    }
    fn touch_channel(&mut self, channel: &mut Self::Channel, t: Option<u64>) {
        use std::time::SystemTime;
        debug!("touch_channel: {:?}", t);
        if let Some(t) = t {
            channel.last_modified = t
        } else if let Ok(duration) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            debug!("touch {:?}", duration.as_secs() * 1000);
            channel.last_modified = duration.as_secs() * 1000
        }
    }

    fn put_changes(
        &mut self,
        channel: &mut Self::Channel,
        p: ChangeId,
        t: ApplyTimestamp,
        h: &Hash,
    ) -> Result<Option<Merkle>, TxnErr<Self::GraphError>> {
        debug!("put_changes {:?} {:?}", p, h);
        if let Some(m) = self.get_changeset(&channel.changes, &p)? {
            debug!("found m = {:?}, p = {:?}", m, p);
            Ok(None)
        } else {
            channel.apply_counter += 1;
            debug!("put_changes {:?} {:?}", t, p);
            let m = if let Some(x) = btree::rev_iter(&self.txn, &channel.revchanges, None)?.next() {
                let (a, b) = x?;
                let a: u64 = (*a).into();
                assert!(a < t);
                (&b.b).into()
            } else {
                Merkle::zero()
            };
            let m = m.next(h);
            assert!(
                self.get_revchangeset(&channel.revchanges, &t.into())?
                    .is_none()
            );
            assert!(btree::put(
                &mut self.txn,
                &mut channel.changes,
                &p,
                &t.into()
            )?);
            assert!(btree::put(
                &mut self.txn,
                &mut channel.revchanges,
                &t.into(),
                &Pair { a: p, b: m.into() }
            )?);
            assert!(btree::put(
                &mut self.txn,
                &mut channel.states,
                &m.into(),
                &t.into(),
            )?);
            Ok(Some(m))
        }
    }

    fn del_changes(
        &mut self,
        channel: &mut Self::Channel,
        p: ChangeId,
        t: ApplyTimestamp,
    ) -> Result<bool, TxnErr<Self::GraphError>> {
        let mut repl = Vec::new();
        let tl = t.into();
        for x in btree::iter(&self.txn, &channel.revchanges, Some((&tl, None)))? {
            let (t_, p) = x?;
            if *t_ >= tl {
                repl.push((*t_, p.a, p.b))
            }
        }
        let mut m = Merkle::zero();
        for x in btree::rev_iter(&self.txn, &channel.revchanges, Some((&tl, None)))? {
            let (t_, mm) = x?;
            if t_ < &tl {
                m = (&mm.b).into();
                break;
            }
        }
        for (t_, p, m0) in repl.iter() {
            debug!("del_changes {:?} {:?}", t_, p);
            btree::del(&mut self.txn, &mut channel.revchanges, t_, None)?;
            btree::del(&mut self.txn, &mut channel.states, m0, None)?;
            if *t_ > tl {
                m = m.next(self.get_external(p).optional()?.unwrap());
                btree::put(
                    &mut self.txn,
                    &mut channel.revchanges,
                    t_,
                    &Pair { a: *p, b: m.into() },
                )?;
                btree::put(&mut self.txn, &mut channel.states, &m.into(), t_)?;
            }
        }
        btree::del(&mut self.txn, &mut channel.tags, &t.into(), None)?;
        Ok(btree::del(
            &mut self.txn,
            &mut channel.changes,
            &p,
            Some(&t.into()),
        )?)
    }

    fn tags_mut<'a>(&mut self, channel: &'a mut Self::Channel) -> &'a mut Self::Tags {
        &mut channel.tags
    }

    fn put_tags(
        &mut self,
        channel: &mut Self::Tags,
        n: u64,
        m: &Merkle,
    ) -> Result<(), TxnErr<Self::GraphError>> {
        debug!("put_tags {:?}", m);
        let mm: SerializedMerkle = m.into();
        if btree::get(&self.txn, channel, &n.into(), None)?.is_some() {
            debug!("already tagged");
            Ok(())
        } else {
            let tl = n.into();
            let mut repl = vec![(tl, mm)];
            replay_tags(self, channel, tl, &mut repl)?;
            Ok(())
        }
    }

    fn del_tags(
        &mut self,
        channel: &mut Self::Tags,
        t: u64,
    ) -> Result<(), TxnErr<Self::GraphError>> {
        replay_tags(self, channel, t.into(), &mut Vec::new())?;
        Ok(())
    }

    fn move_change(
        &mut self,
        channel: &mut Self::Channel,
        change_id: ChangeId,
        old_pos: ApplyTimestamp,
        new_pos: ApplyTimestamp,
    ) -> Result<(), TxnErr<Self::GraphError>> {
        if old_pos == new_pos {
            return Ok(());
        }
        let new_l64 = L64::from(new_pos);
        let old_l64 = L64::from(old_pos);
        // Collect all entries in [new_pos, old_pos] — the range being reordered.
        let mut entries: Vec<(L64, ChangeId)> = Vec::new();
        for x in btree::iter(&self.txn, &channel.revchanges, Some((&new_l64, None)))? {
            let (t, p) = x?;
            let t_u64: u64 = (*t).into();
            if t_u64 > old_pos {
                break;
            }
            entries.push((*t, p.a));
        }
        // Remove them all from revchanges, states, and changes.
        let mut merkles: Vec<SerializedMerkle> = Vec::with_capacity(entries.len());
        for (t, _) in entries.iter() {
            let m = self.get_revchangeset(&channel.revchanges, t)?.unwrap().b;
            merkles.push(m);
        }
        for ((t, p), m) in entries.iter().zip(merkles.iter()) {
            btree::del(&mut self.txn, &mut channel.revchanges, t, None)?;
            btree::del(&mut self.txn, &mut channel.states, m, None)?;
            btree::del(&mut self.txn, &mut channel.changes, p, Some(t))?;
        }
        // Starting Merkle is the value just before new_pos.
        let mut m: Merkle = {
            let mut it = btree::rev_iter(&self.txn, &channel.revchanges, Some((&new_l64, None)))?;
            match it.next() {
                Some(x) => {
                    let (t_, mm) = x?;
                    if *t_ < new_l64 {
                        (&mm.b).into()
                    } else {
                        Merkle::zero()
                    }
                }
                None => Merkle::zero(),
            }
        };
        // New order: change_id first (it was at old_pos), then entries[0..last] in original order.
        let last = entries.len() - 1;
        let new_order: Vec<ChangeId> = std::iter::once(change_id)
            .chain(entries[..last].iter().map(|(_, p)| *p))
            .collect();
        for (i, p) in new_order.iter().enumerate() {
            let pos = L64::from(new_pos + i as u64);
            m = m.next(self.get_external(p).optional()?.unwrap());
            btree::put(
                &mut self.txn,
                &mut channel.revchanges,
                &pos,
                &Pair { a: *p, b: m.into() },
            )?;
            btree::put(&mut self.txn, &mut channel.states, &m.into(), &pos)?;
            btree::put(&mut self.txn, &mut channel.changes, p, &pos)?;
        }
        let _ = old_l64;
        Ok(())
    }
}

fn replay_tags<
    T: sanakirja::AllocPage<Error = ::sanakirja::Error>
        + sanakirja::RootPage
        + sanakirja::LoadPage<Error = ::sanakirja::Error>,
>(
    txn: &mut MutTxn<T>,
    channel: &mut Db<L64, Pair<SerializedMerkle, SerializedMerkle>>,
    tl: L64,
    repl: &mut Vec<(L64, SerializedMerkle)>,
) -> Result<(), TxnErr<SanakirjaError>> {
    let del = repl.is_empty();
    for x in btree::iter(&txn.txn, channel, Some((&tl, None)))? {
        let (t_, p) = x?;
        if *t_ >= tl {
            repl.push((*t_, p.a))
        }
    }
    let mut m = Merkle::zero();
    for x in btree::rev_iter(&txn.txn, channel, Some((&tl, None)))? {
        let (t_, mm) = x?;
        if t_ < &tl {
            m = (&mm.b).into();
            break;
        }
    }
    for (t_, p) in repl.iter() {
        debug!("del_tags {:?} {:?}", t_, p);
        btree::del(&mut txn.txn, channel, t_, None)?;
        if *t_ > tl || !del {
            m = m.next(p);
            btree::put(&mut txn.txn, channel, t_, &Pair { a: *p, b: m.into() })?;
        }
    }
    Ok(())
}

impl<
    T: sanakirja::AllocPage<Error = ::sanakirja::Error>
        + sanakirja::RootPage
        + sanakirja::LoadPage<Error = ::sanakirja::Error>,
> DepsMutTxnT for MutTxn<T>
{
    sanakirja_put_del!(dep, ChangeId, ChangeId, DepsError);
    sanakirja_put_del!(revdep, ChangeId, ChangeId, DepsError);
    sanakirja_put_del!(touched_files, Position<ChangeId>, ChangeId, DepsError);
    sanakirja_put_del!(rev_touched_files, ChangeId, Position<ChangeId>, DepsError);
}

impl<
    T: sanakirja::AllocPage<Error = ::sanakirja::Error>
        + sanakirja::RootPageMut
        + sanakirja::LoadPage<Error = ::sanakirja::Error>,
> TreeMutTxnT for MutTxn<T>
{
    // Hand-written (the `inodes` value is `SerializedInode` since format v2):
    // `put_inodes` takes a `Position` and initialises the stat cache to unknown;
    // `del_inodes` deletes by key. Stat is refreshed via `set_inode_stat`.
    fn put_inodes(
        &mut self,
        k: &Inode,
        v: &Position<ChangeId>,
    ) -> Result<bool, TreeErr<Self::TreeError>> {
        // Preserve an existing stat if the position is unchanged, otherwise reset
        // it to "unknown" so the file is re-read once.
        let value = match ::sanakirja::btree::get(&self.txn, &self.inodes, k, None) {
            Ok(Some((kk, old))) if kk == k && old.position == *v => *old,
            _ => SerializedInode::new(*v),
        };
        // `put` on an existing key replaces; make sure we don't leave a stale one.
        ::sanakirja::btree::del(&mut self.txn, &mut self.inodes, k, None).map_err(TreeErr)?;
        Ok(::sanakirja::btree::put(&mut self.txn, &mut self.inodes, k, &value).map_err(TreeErr)?)
    }
    fn del_inodes(
        &mut self,
        k: &Inode,
        _v: Option<&Position<ChangeId>>,
    ) -> Result<bool, TreeErr<Self::TreeError>> {
        Ok(::sanakirja::btree::del(&mut self.txn, &mut self.inodes, k, None).map_err(TreeErr)?)
    }
    fn set_inode_stat(
        &mut self,
        k: &Inode,
        mtime: u64,
        size: u64,
    ) -> Result<(), TreeErr<Self::TreeError>> {
        let position = match ::sanakirja::btree::get(&self.txn, &self.inodes, k, None) {
            Ok(Some((kk, v))) if kk == k => v.position,
            _ => return Ok(()), // no such inode: nothing to cache
        };
        let value = SerializedInode {
            position,
            mtime: L64(mtime),
            size: L64(size),
        };
        ::sanakirja::btree::del(&mut self.txn, &mut self.inodes, k, None).map_err(TreeErr)?;
        ::sanakirja::btree::put(&mut self.txn, &mut self.inodes, k, &value).map_err(TreeErr)?;
        Ok(())
    }
    sanakirja_put_del!(revinodes, Position<ChangeId>, Inode, TreeError, TreeErr);

    sanakirja_put_del!(tree, PathId, Inode, TreeError, TreeErr);
    sanakirja_put_del!(revtree, Inode, PathId, TreeError, TreeErr);

    fn put_partials(
        &mut self,
        k: &SmallStr,
        e: Position<ChangeId>,
    ) -> Result<bool, TreeErr<Self::TreeError>> {
        btree::put(&mut self.txn, &mut self.partials, k, &e).map_err(|e| TreeErr(e.into()))
    }

    fn del_partials(
        &mut self,
        k: &SmallStr,
        e: Option<Position<ChangeId>>,
    ) -> Result<bool, TreeErr<Self::TreeError>> {
        btree::del(&mut self.txn, &mut self.partials, k, e.as_ref()).map_err(|e| TreeErr(e.into()))
    }
}

impl<T: RawMutTxnT> MutTxnT for MutTxn<T> {
    fn mark_superseded(
        &mut self,
        pred: &Hash,
        succ: &Hash,
    ) -> Result<(), TxnErr<Self::GraphError>> {
        // Create the table lazily on the first amend (see `is_superseded`).
        let mut db = match self.superseded.take() {
            Some(db) => db,
            None => unsafe { btree::create_db_(&mut self.txn)? },
        };
        let (pk, sk): (SerializedHash, SerializedHash) = (pred.into(), succ.into());
        btree::put(&mut self.txn, &mut db, &pk, &sk)?;
        self.txn.set_root(Root::Superseded as usize, db.db.into());
        self.superseded = Some(db);
        Ok(())
    }

    fn unmark_superseded(
        &mut self,
        pred: &Hash,
        succ: &Hash,
    ) -> Result<(), TxnErr<Self::GraphError>> {
        let mut db = match self.superseded.take() {
            Some(db) => db,
            None => return Ok(()),
        };
        // Delete only the exact `pred -> succ` pair; the table is multi-valued
        // (concurrent amends can supersede the same `pred`), so a keyed-only
        // delete would wrongly drop siblings.
        let (pk, sk): (SerializedHash, SerializedHash) = (pred.into(), succ.into());
        btree::del(&mut self.txn, &mut db, &pk, Some(&sk))?;
        self.txn.set_root(Root::Superseded as usize, db.db.into());
        self.superseded = Some(db);
        Ok(())
    }

    fn put_remote(
        &mut self,
        remote: &mut RemoteRef<Self>,
        k: u64,
        v: (Hash, Merkle),
    ) -> Result<bool, TxnErr<Self::GraphError>> {
        let mut remote = remote.db.lock();
        let h = (&v.0).into();
        let m: SerializedMerkle = (&v.1).into();
        btree::put(
            &mut self.txn,
            &mut remote.remote,
            &k.into(),
            &Pair { a: h, b: m },
        )?;
        debug!("remote.remote after put: {:?}", remote.remote);
        btree::put(&mut self.txn, &mut remote.states, &m, &k.into())?;
        // if v.2 {
        //     self.put_tags(&mut remote.tags, k, &v.1)?;
        // }
        Ok(btree::put(&mut self.txn, &mut remote.rev, &h, &k.into())?)
    }

    fn del_remote(
        &mut self,
        remote: &mut RemoteRef<Self>,
        k: u64,
    ) -> Result<bool, TxnErr<Self::GraphError>> {
        let mut remote = remote.db.lock();
        let k = k.into();
        match btree::get(&self.txn, &remote.remote, &k, None)? {
            Some((k0, p)) if k0 == &k => {
                debug!("del_remote {:?} {:?}", k0, p);
                let p = *p;
                btree::del(&mut self.txn, &mut remote.rev, &p.a, None)?;
                btree::del(&mut self.txn, &mut remote.states, &p.b, None)?;
                Ok(btree::del(&mut self.txn, &mut remote.remote, &k, None)?)
            }
            x => {
                debug!("not found, {:?}", x);
                Ok(false)
            }
        }
    }

    fn open_or_create_channel(
        &mut self,
        name: &SmallStr,
    ) -> Result<ChannelRef<Self>, Self::GraphError> {
        unsafe {
            let mut commit = None;
            let result = match self.open_channels.lock().entry(name.to_owned()) {
                Entry::Vacant(v) => {
                    let r = match btree::get(&self.txn, &self.channels, name, None)? {
                        Some((name_, b)) if name_ == name => ChannelRef {
                            r: Arc::new(RwLock::new(Channel {
                                graph: Db::from_page(b.graph.into()),
                                changes: Db::from_page(b.changes.into()),
                                revchanges: UDb::from_page(b.revchanges.into()),
                                states: UDb::from_page(b.states.into()),
                                tags: Db::from_page(b.tags.into()),
                                apply_counter: b.apply_counter.into(),
                                last_modified: b.last_modified.into(),
                                id: b.id,
                                name: name.to_owned(),
                            })),
                        },
                        _ => {
                            let br = ChannelRef {
                                r: Arc::new(RwLock::new(Channel {
                                    graph: btree::create_db_(&mut self.txn)?,
                                    changes: btree::create_db_(&mut self.txn)?,
                                    revchanges: btree::create_db_(&mut self.txn)?,
                                    states: btree::create_db_(&mut self.txn)?,
                                    tags: btree::create_db_(&mut self.txn)?,
                                    id: {
                                        let mut rng = rand::rng();
                                        use rand::RngExt;
                                        let mut x = RemoteId([0; 16]);
                                        for x in x.0.iter_mut() {
                                            *x = rng.random()
                                        }
                                        x
                                    },
                                    apply_counter: 0,
                                    last_modified: 0,
                                    name: name.to_owned(),
                                })),
                            };
                            commit = Some(br.clone());
                            br
                        }
                    };
                    v.insert(r).clone()
                }
                Entry::Occupied(occ) => occ.get().clone(),
            };
            if let Some(commit) = commit {
                self.put_channel(commit)?;
            }
            Ok(result)
        }
    }

    fn fork(
        &mut self,
        channel: &ChannelRef<Self>,
        name: &SmallStr,
    ) -> Result<ChannelRef<Self>, ForkError<Self::GraphError>> {
        let channel = channel.r.read();
        match btree::get(&self.txn, &self.channels, name, None)
            .map_err(|e| ForkError::Txn(e.into()))?
        {
            Some((name_, _)) if name_ == name => {
                Err(super::ForkError::ChannelNameExists(name.to_string()))
            }
            _ => {
                let br = ChannelRef {
                    r: Arc::new(RwLock::new(Channel {
                        graph: btree::fork_db(&mut self.txn, &channel.graph)
                            .map_err(|e| ForkError::Txn(e.into()))?,
                        changes: btree::fork_db(&mut self.txn, &channel.changes)
                            .map_err(|e| ForkError::Txn(e.into()))?,
                        revchanges: btree::fork_db(&mut self.txn, &channel.revchanges)
                            .map_err(|e| ForkError::Txn(e.into()))?,
                        states: btree::fork_db(&mut self.txn, &channel.states)
                            .map_err(|e| ForkError::Txn(e.into()))?,
                        tags: btree::fork_db(&mut self.txn, &channel.tags)
                            .map_err(|e| ForkError::Txn(e.into()))?,
                        name: name.to_owned(),
                        apply_counter: channel.apply_counter,
                        last_modified: channel.last_modified,
                        id: {
                            let mut rng = rand::rng();
                            use rand::RngExt;
                            let mut x = RemoteId([0; 16]);
                            for x in x.0.iter_mut() {
                                *x = rng.random()
                            }
                            x
                        },
                    })),
                };
                self.open_channels
                    .lock()
                    .insert(name.to_owned(), br.clone());
                Ok(br)
            }
        }
    }

    fn rename_channel(
        &mut self,
        channel: &mut ChannelRef<Self>,
        name: &SmallStr,
    ) -> Result<(), ForkError<Self::GraphError>> {
        match btree::get(&self.txn, &self.channels, name, None)
            .map_err(|e| ForkError::Txn(e.into()))?
        {
            Some((name_, _)) if name_ == name => {
                Err(super::ForkError::ChannelNameExists(name.to_string()))
            }
            _ => {
                btree::del(
                    &mut self.txn,
                    &mut self.channels,
                    &channel.r.read().name,
                    None,
                )
                .map_err(|e| ForkError::Txn(e.into()))?;
                std::mem::drop(
                    self.open_channels
                        .lock()
                        .remove(&channel.r.read().name)
                        .unwrap(),
                );
                channel.r.write().name = name.to_owned();
                self.open_channels
                    .lock()
                    .insert(name.to_owned(), channel.clone());
                Ok(())
            }
        }
    }

    fn drop_channel(&mut self, name: &SmallStr) -> Result<bool, Self::GraphError> {
        unsafe {
            let name = name.to_owned();
            debug!(target: "drop_channel", "drop channel {:?}", name);
            let channel = if let Some(channel) = self.open_channels.lock().remove(&name) {
                let channel = Arc::try_unwrap(channel.r)
                    .map_err(|_| SanakirjaError::ChannelRc {
                        c: name.to_string(),
                    })?
                    .into_inner();
                Some((
                    channel.graph,
                    channel.changes,
                    channel.revchanges,
                    channel.states,
                    channel.tags,
                ))
            } else if let Some((name_, chan)) = btree::get(&self.txn, &self.channels, &name, None)?
            {
                if name_ == name.as_ref() {
                    Some((
                        Db::from_page(chan.graph.into()),
                        Db::from_page(chan.changes.into()),
                        UDb::from_page(chan.revchanges.into()),
                        UDb::from_page(chan.states.into()),
                        Db::from_page(chan.tags.into()),
                    ))
                } else {
                    None
                }
            } else {
                None
            };
            btree::del(&mut self.txn, &mut self.channels, &name, None)?;
            if let Some((a, b, c, d, e)) = channel {
                let mut unused_changes = Vec::new();
                'outer: for x in btree::rev_iter(&self.txn, &c, None)? {
                    let (_, p) = x?;
                    debug!(target: "drop_channel", "testing unused change: {:?}", p);
                    let empty = SmallString::new();
                    for chan in self.channels(&empty).map_err(|e| e.0)? {
                        debug!(target: "drop_channel", "channel: {:?}", name);
                        let chan = chan.read();
                        assert_ne!(chan.name, name);
                        if self
                            .channel_has_state(&chan.states, &p.b)
                            .map_err(|e| e.0)?
                            .is_some()
                        {
                            // This other channel is in the same state as
                            // our dropped channel is, so all subsequent
                            // patches are in use.
                            break 'outer;
                        }
                        if self
                            .get_changeset(&chan.changes, &p.a)
                            .map_err(|e| e.0)?
                            .is_some()
                        {
                            // This channel has a patch, move on.
                            continue 'outer;
                        }
                    }

                    debug!(target: "drop_channel", "actually unused: {:?}", p);
                    unused_changes.push(p.a);
                }
                let mut deps = Vec::new();
                for ch in unused_changes.iter() {
                    for x in btree::iter(&self.txn, &self.dep, Some((ch, None)))? {
                        let (k, v) = x?;
                        if k > ch {
                            break;
                        }
                        deps.push((*k, *v));
                    }
                    for (k, v) in deps.drain(..) {
                        debug!(target: "drop_channel", "deleting from revdep: {:?} {:?}", k, v);
                        btree::del(&mut self.txn, &mut self.dep, &k, Some(&v))?;
                        btree::del(&mut self.txn, &mut self.revdep, &v, Some(&k))?;
                    }
                }
                btree::drop(&mut self.txn, a)?;
                btree::drop(&mut self.txn, b)?;
                btree::drop(&mut self.txn, c)?;
                btree::drop(&mut self.txn, d)?;
                btree::drop(&mut self.txn, e)?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    fn open_or_create_remote(
        &mut self,
        id: RemoteId,
        path: &SmallStr,
    ) -> Result<RemoteRef<Self>, Self::GraphError> {
        unsafe {
            let mut commit = None;
            match self.open_remotes.lock().entry(id) {
                Entry::Vacant(v) => {
                    let r = match btree::get(&self.txn, &self.remotes, &id, None)? {
                        Some((name_, remote)) if *name_ == id => RemoteRef {
                            db: Arc::new(Mutex::new(Remote {
                                remote: UDb::from_page(remote.remote.into()),
                                rev: UDb::from_page(remote.rev.into()),
                                states: UDb::from_page(remote.states.into()),
                                id_rev: remote.id_rev,
                                tags: Db::from_page(remote.tags.into()),
                                path: path.to_owned(),
                            })),
                            id,
                        },
                        _ => {
                            let br = RemoteRef {
                                db: Arc::new(Mutex::new(Remote {
                                    remote: btree::create_db_(&mut self.txn)?,
                                    rev: btree::create_db_(&mut self.txn)?,
                                    states: btree::create_db_(&mut self.txn)?,
                                    id_rev: 0u64.into(),
                                    tags: btree::create_db(&mut self.txn)?,
                                    path: path.to_owned(),
                                })),
                                id,
                            };
                            commit = Some(br.clone());
                            br
                        }
                    };
                    v.insert(r);
                }
                Entry::Occupied(_) => {}
            }
            if let Some(commit) = commit {
                self.put_remotes(commit)?;
            }
            Ok(self.open_remotes.lock().get(&id).unwrap().clone())
        }
    }

    fn drop_remote(&mut self, remote: RemoteRef<Self>) -> Result<bool, Self::GraphError> {
        let r = self.open_remotes.lock().remove(&remote.id).unwrap();
        std::mem::drop(remote);
        assert_eq!(Arc::strong_count(&r.db), 1);
        Ok(btree::del(&mut self.txn, &mut self.remotes, &r.id, None)?)
    }

    fn drop_named_remote(&mut self, id: RemoteId) -> Result<bool, Self::GraphError> {
        if let Some(r) = self.open_remotes.lock().remove(&id) {
            assert_eq!(Arc::strong_count(&r.db), 1);
        }
        Ok(btree::del(&mut self.txn, &mut self.remotes, &id, None)?)
    }

    fn commit(mut self) -> Result<(), Self::GraphError> {
        use std::ops::DerefMut;
        {
            let open_channels = std::mem::take(self.open_channels.lock().deref_mut());
            for (name, channel) in open_channels {
                debug!("commit_channel {:?}", name);
                self.commit_channel(channel)?
            }
        }
        {
            let open_remotes = std::mem::take(self.open_remotes.lock().deref_mut());
            for (name, remote) in open_remotes {
                debug!("commit remote {:?}", name);
                self.commit_remote(remote)?
            }
        }
        if let Some(ref cur) = self.cur_channel {
            unsafe {
                assert!(cur.len() < 256);
                let b = self.txn.root_page_mut();
                b[4096 - 256] = cur.len() as u8;
                std::ptr::copy(cur.as_ptr(), b.as_mut_ptr().add(4096 - 255), cur.len())
            }
        }
        // No need to set `Root::Version`, it is set at init.
        debug!(
            "{:x} {:x} {:x} {:x} {:x} {:x} {:x} {:x} {:x} {:x} {:x} {:x} {:x}",
            self.tree.db,
            self.revtree.db,
            self.inodes.db,
            self.revinodes.db,
            self.internal.db,
            self.external.db,
            self.revdep.db,
            self.channels.db,
            self.remotes.db,
            self.touched_files.db,
            self.dep.db,
            self.rev_touched_files.db,
            self.partials.db,
        );
        self.txn
            .set_root(Root::Tree as usize, u64::from(self.tree.db));
        self.txn
            .set_root(Root::RevTree as usize, u64::from(self.revtree.db));
        self.txn
            .set_root(Root::Inodes as usize, u64::from(self.inodes.db));
        self.txn
            .set_root(Root::RevInodes as usize, self.revinodes.db.into());
        self.txn
            .set_root(Root::Internal as usize, self.internal.db.into());
        self.txn
            .set_root(Root::External as usize, self.external.db.into());
        self.txn
            .set_root(Root::RevDep as usize, self.revdep.db.into());
        self.txn
            .set_root(Root::Channels as usize, self.channels.db.into());
        self.txn
            .set_root(Root::Remotes as usize, self.remotes.db.into());
        self.txn
            .set_root(Root::TouchedFiles as usize, self.touched_files.db.into());
        self.txn.set_root(Root::Dep as usize, self.dep.db.into());
        self.txn.set_root(
            Root::RevTouchedFiles as usize,
            self.rev_touched_files.db.into(),
        );
        self.txn
            .set_root(Root::Partials as usize, self.partials.db.into());
        if let Some(ref db) = self.superseded {
            self.txn.set_root(Root::Superseded as usize, db.db.into());
        }
        self.txn.commit()?;
        Ok(())
    }

    fn set_current_channel(&mut self, cur: &str) -> Result<(), Self::GraphError> {
        self.cur_channel = Some(cur.to_string());
        Ok(())
    }
}

impl Txn {
    pub fn load_const_channel(&self, name: &SmallStr) -> Result<Option<Channel>, SanakirjaError> {
        unsafe {
            let name = name.to_owned();
            match btree::get(&self.txn, &self.channels, &name, None)? {
                Some((name_, c)) if name.as_ref() == name_ => {
                    debug!("load_const_channel = {:?} {:?}", name_, c);
                    Ok(Some(Channel {
                        graph: Db::from_page(c.graph.into()),
                        changes: Db::from_page(c.changes.into()),
                        revchanges: UDb::from_page(c.revchanges.into()),
                        states: UDb::from_page(c.states.into()),
                        tags: Db::from_page(c.tags.into()),
                        apply_counter: c.apply_counter.into(),
                        last_modified: c.last_modified.into(),
                        id: c.id,
                        name,
                    }))
                }
                _ => Ok(None),
            }
        }
    }
}

impl<
    T: sanakirja::AllocPage<Error = ::sanakirja::Error>
        + sanakirja::RootPage
        + sanakirja::LoadPage<Error = ::sanakirja::Error>,
> MutTxn<T>
{
    fn put_channel(&mut self, channel: ChannelRef<Self>) -> Result<(), SanakirjaError> {
        debug!("Commit_channel.");
        let channel = channel.r.read();
        debug!("Commit_channel, dbs_channels = {:?}", self.channels);
        btree::del(&mut self.txn, &mut self.channels, &channel.name, None)?;
        debug!(
            "channels: {:x} {:x} {:x} {:x} {:x}",
            channel.graph.db,
            channel.changes.db,
            channel.revchanges.db,
            channel.states.db,
            channel.tags.db,
        );
        let sc = SerializedChannel {
            graph: u64::from(channel.graph.db).into(),
            changes: u64::from(channel.changes.db).into(),
            revchanges: u64::from(channel.revchanges.db).into(),
            states: u64::from(channel.states.db).into(),
            tags: u64::from(channel.tags.db).into(),
            apply_counter: channel.apply_counter.into(),
            last_modified: channel.last_modified.into(),
            id: channel.id,
        };
        btree::put(&mut self.txn, &mut self.channels, &channel.name, &sc)?;
        debug!("Commit_channel, self.channels = {:?}", self.channels);
        Ok(())
    }

    fn commit_channel(&mut self, channel: ChannelRef<Self>) -> Result<(), SanakirjaError> {
        std::mem::drop(self.open_channels.lock().remove(&channel.r.read().name));
        self.put_channel(channel)
    }

    fn put_remotes(&mut self, remote: RemoteRef<Self>) -> Result<(), SanakirjaError> {
        btree::del(&mut self.txn, &mut self.remotes, &remote.id, None)?;
        debug!("Commit_remote, dbs_remotes = {:?}", self.remotes);
        let r = remote.db.lock();
        let rr = OwnedSerializedRemote {
            _remote: u64::from(r.remote.db).into(),
            _rev: u64::from(r.rev.db).into(),
            _states: u64::from(r.states.db).into(),
            _id_rev: r.id_rev,
            _tags: u64::from(r.tags.db).into(),
            _path: r.path.clone(),
        };
        debug!("put {:?}", rr);
        btree::put(&mut self.txn, &mut self.remotes, &remote.id, &rr)?;
        debug!("Commit_remote, self.dbs.remotes = {:?}", self.remotes);
        Ok(())
    }

    fn commit_remote(&mut self, remote: RemoteRef<Self>) -> Result<(), SanakirjaError> {
        std::mem::drop(self.open_remotes.lock().remove(&remote.id));
        // assert_eq!(Rc::strong_count(&remote.db), 1);
        self.put_remotes(remote)
    }
}

direct_repr!(ChangeId);
impl ::sanakirja::debug::Check for ChangeId {}

direct_repr!(Vertex<ChangeId>);
impl ::sanakirja::debug::Check for Vertex<ChangeId> {}

direct_repr!(Position<ChangeId>);
impl ::sanakirja::debug::Check for Position<ChangeId> {}

direct_repr!(SerializedInode);
impl ::sanakirja::debug::Check for SerializedInode {}

direct_repr!(SerializedEdge);
impl ::sanakirja::debug::Check for SerializedEdge {}

impl ::sanakirja::debug::Check for PathId {}
impl Storable for PathId {
    fn compare<T>(&self, _: &T, x: &Self) -> std::cmp::Ordering {
        self.cmp(x)
    }
    type PageReferences = std::iter::Empty<u64>;
    fn page_references(&self) -> Self::PageReferences {
        std::iter::empty()
    }
}
impl UnsizedStorable for PathId {
    const ALIGN: usize = 8;
    fn size(&self) -> usize {
        9 + self.basename.len()
    }
    unsafe fn onpage_size(p: *const u8) -> usize {
        unsafe {
            let len = *(p.add(8)) as usize;
            9 + len
        }
    }
    unsafe fn from_raw_ptr<'a, T>(_: &T, p: *const u8) -> &'a Self {
        unsafe { path_id_from_raw_ptr(p) }
    }
    unsafe fn write_to_page(&self, p: *mut u8) {
        unsafe {
            *(p as *mut u64) = (self.parent_inode.0).0;
            self.basename.write_to_page(p.add(8))
        }
    }
}

unsafe fn path_id_from_raw_ptr<'a>(p: *const u8) -> &'a PathId {
    unsafe {
        let len = *(p.add(8)) as usize;
        std::mem::transmute(std::slice::from_raw_parts(p, 1 + len))
    }
}

#[test]
fn pathid_repr() {
    let o = OwnedPathId {
        parent_inode: Inode::ROOT,
        basename: SmallString::from_str("blablabla"),
    };
    let mut x = vec![0u8; 200];

    unsafe {
        o.write_to_page(x.as_mut_ptr());
        let p = path_id_from_raw_ptr(x.as_ptr());
        assert_eq!(p.basename.as_str(), "blablabla");
        assert_eq!(p.parent_inode, Inode::ROOT);
    }
}

direct_repr!(Inode);
impl ::sanakirja::debug::Check for Inode {}
direct_repr!(SerializedMerkle);
impl ::sanakirja::debug::Check for SerializedMerkle {}
direct_repr!(SerializedHash);
impl ::sanakirja::debug::Check for SerializedHash {}

impl<A: ::sanakirja::debug::Check, B: ::sanakirja::debug::Check> ::sanakirja::debug::Check
    for Pair<A, B>
{
    fn add_refs<T: LoadPage>(
        &self,
        txn: &T,
        pages: &mut std::collections::BTreeMap<u64, usize>,
    ) -> Result<(), T::Error>
    where
        T::Error: std::fmt::Debug,
    {
        self.a.add_refs(txn, pages)?;
        self.b.add_refs(txn, pages)
    }
}
impl<A: Storable, B: Storable> Storable for Pair<A, B> {
    type PageReferences = core::iter::Chain<A::PageReferences, B::PageReferences>;
    fn page_references(&self) -> Self::PageReferences {
        self.a.page_references().chain(self.b.page_references())
    }
    fn compare<T: LoadPage>(&self, t: &T, b: &Self) -> core::cmp::Ordering {
        match self.a.compare(t, &b.a) {
            core::cmp::Ordering::Equal => self.b.compare(t, &b.b),
            ord => ord,
        }
    }
}

impl<A: Ord + UnsizedStorable, B: Ord + UnsizedStorable> UnsizedStorable for Pair<A, B> {
    const ALIGN: usize = std::mem::align_of::<(A, B)>();

    fn size(&self) -> usize {
        let a = self.a.size();
        let b_off = (a + (B::ALIGN - 1)) & !(B::ALIGN - 1);
        (b_off + self.b.size() + (Self::ALIGN - 1)) & !(Self::ALIGN - 1)
    }
    unsafe fn onpage_size(p: *const u8) -> usize {
        unsafe {
            let a = A::onpage_size(p);
            let b_off = (a + (B::ALIGN - 1)) & !(B::ALIGN - 1);
            let b_size = B::onpage_size(p.add(b_off));
            (b_off + b_size + (Self::ALIGN - 1)) & !(Self::ALIGN - 1)
        }
    }
    unsafe fn from_raw_ptr<'a, T>(_: &T, p: *const u8) -> &'a Self {
        unsafe { &*(p as *const Self) }
    }
    unsafe fn write_to_page_alloc<T: sanakirja::AllocPage>(&self, t: &mut T, p: *mut u8) {
        unsafe {
            self.a.write_to_page_alloc(t, p);
            let off = (self.a.size() + (B::ALIGN - 1)) & !(B::ALIGN - 1);
            self.b.write_to_page_alloc(t, p.add(off));
        }
    }
}

impl ::sanakirja::debug::Check for SerializedRemote {}
impl Storable for SerializedRemote {
    type PageReferences = std::iter::Empty<u64>;
    fn page_references(&self) -> Self::PageReferences {
        std::iter::empty()
    }
    fn compare<T: LoadPage>(&self, _t: &T, b: &Self) -> core::cmp::Ordering {
        self.cmp(b)
    }
}

const REMOTE_LEN: usize = 40;

impl UnsizedStorable for SerializedRemote {
    const ALIGN: usize = 8;

    fn size(&self) -> usize {
        REMOTE_LEN + 1 + self.path.len()
    }
    unsafe fn onpage_size(p: *const u8) -> usize {
        unsafe { REMOTE_LEN + 1 + (*p.add(REMOTE_LEN)) as usize }
    }
    unsafe fn from_raw_ptr<'a, T>(_: &T, p: *const u8) -> &'a Self {
        unsafe {
            let len = *p.add(REMOTE_LEN) as usize;
            let m: &SerializedRemote = std::mem::transmute(std::slice::from_raw_parts(p, 1 + len));
            m
        }
    }
    unsafe fn write_to_page_alloc<T: sanakirja::AllocPage>(&self, _: &mut T, p: *mut u8) {
        unsafe {
            std::ptr::copy(
                &self.remote as *const L64 as *const u8,
                p,
                REMOTE_LEN + 1 + self.path.len(),
            );
            debug!(
                "write_to_page: {:?}",
                std::slice::from_raw_parts(p, REMOTE_LEN + 1 + self.path.len())
            );
        }
    }
}

#[derive(Debug)]
#[repr(C)]
struct OwnedSerializedRemote {
    _remote: L64,
    _rev: L64,
    _states: L64,
    _id_rev: L64,
    _tags: L64,
    _path: SmallString,
}

impl std::ops::Deref for OwnedSerializedRemote {
    type Target = SerializedRemote;
    fn deref(&self) -> &Self::Target {
        let len = REMOTE_LEN + 1 + self._path.len();
        unsafe {
            std::mem::transmute(std::slice::from_raw_parts(
                self as *const Self as *const u8,
                len,
            ))
        }
    }
}

direct_repr!(SerializedChannel);
impl ::sanakirja::debug::Check for SerializedChannel {}

direct_repr!(RemoteId);
impl ::sanakirja::debug::Check for RemoteId {}
