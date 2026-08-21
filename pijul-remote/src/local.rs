use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use log::debug;
use pijul_core::pristine::{Hash, Merkle, MutTxnT, Position, TxnT};
use pijul_core::*;

use crate::CS;
use crate::error::RemoteError;
use pijul_interaction::ProgressBar;

#[derive(Clone)]
pub struct Local {
    pub channel: String,
    pub root: std::path::PathBuf,
    pub changes_dir: std::path::PathBuf,
    pub pristine: Arc<pijul_core::pristine::sanakirja::Pristine>,
    pub name: String,
}

pub fn get_state<T: TxnTExt>(
    txn: &T,
    channel: &pijul_core::pristine::ChannelRef<T>,
    mid: Option<u64>,
) -> Result<Option<(u64, Merkle, Merkle)>, RemoteError>
where
    RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
{
    if let Some(x) = txn.reverse_log(&*channel.read(), mid)?.next() {
        let (n, (_, m)) = x?;
        if let Some(m2) = txn
            .rev_iter_tags(txn.tags(&*channel.read()), Some(n.into()))?
            .next()
        {
            let (_, m2) = m2?;
            Ok(Some((n, m.into(), m2.b.into())))
        } else {
            Ok(Some((n, m.into(), Merkle::zero())))
        }
    } else {
        Ok(None)
    }
}

impl Local {
    pub fn get_state(
        &mut self,
        mid: Option<u64>,
    ) -> Result<Option<(u64, Merkle, Merkle)>, RemoteError> {
        let txn = self.pristine.txn_begin()?;
        let channel = txn.load_channel(self.channel.parse()?)?.unwrap();
        Ok(get_state(&txn, &channel, mid)?)
    }

    pub fn get_id(&self) -> Result<pijul_core::pristine::RemoteId, RemoteError> {
        let txn = self.pristine.txn_begin()?;
        if let Some(channel) = txn.load_channel(self.channel.parse()?)? {
            Ok(*txn.id(&*channel.read()).unwrap())
        } else {
            Err(RemoteError::LocalChannelNotFound(
                self.channel.clone(),
                self.name.clone(),
            ))
        }
    }

    pub fn download_changelist<
        A,
        F: FnMut(&mut A, u64, Hash, Merkle, bool) -> Result<(), RemoteError>,
    >(
        &mut self,
        f: F,
        a: &mut A,
        from: u64,
        paths: &[String],
    ) -> Result<HashSet<Position<Hash>>, RemoteError> {
        let remote_txn = self.pristine.txn_begin()?;
        let remote_channel =
            if let Some(channel) = remote_txn.load_channel(self.channel.parse()?)? {
                channel
            } else {
                debug!(
                    "Local::download_changelist found no channel named {:?}",
                    self.channel
                );
                return Err(RemoteError::LocalChannelNotFound(
                    self.channel.clone(),
                    self.name.clone(),
                ));
            };
        self.download_changelist_(f, a, from, paths, &remote_txn, &remote_channel)
    }

    pub fn download_changelist_<
        A,
        T: pijul_core::ChannelTxnT
            + pijul_core::TxnTExt
            + pijul_core::DepsTxnT
            + pijul_core::GraphTxnT,
        F: FnMut(&mut A, u64, Hash, Merkle, bool) -> Result<(), RemoteError>,
    >(
        &mut self,
        mut f: F,
        a: &mut A,
        from: u64,
        paths: &[String],
        remote_txn: &T,
        remote_channel: &ChannelRef<T>,
    ) -> Result<HashSet<Position<Hash>>, RemoteError>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        let store = pijul_core::changestore::filesystem::FileSystem::from_root(
            &self.root,
            pijul_repository::max_files(),
        );
        let mut paths_ = HashSet::new();
        let mut result = HashSet::new();
        for s in paths {
            if let Ok((p, _ambiguous)) = remote_txn.follow_oldest_path(&store, &remote_channel, s) {
                debug!("p = {:?}", p);

                for p in std::iter::once(p).chain(
                    pijul_core::fs::iter_graph_descendants(
                        remote_txn,
                        remote_txn.graph(&*remote_channel.read()),
                        p,
                    )?
                    .map(|x| x.unwrap()),
                ) {
                    paths_.insert(p);
                    result.insert(Position {
                        change: remote_txn
                            .get_external(&p.change)
                            .optional()?
                            .unwrap()
                            .into(),
                        pos: p.pos,
                    });
                }
            }
        }
        debug!("paths_ = {:?}", paths_);
        debug!("from = {:?}", from);

        let rem = remote_channel.read();
        let tags: Vec<u64> = remote_txn
            .iter_tags(remote_txn.tags(&*rem), from)?
            .map(|k| (*k.unwrap().0).into())
            .collect();
        let mut tagsi = 0;

        if paths_.is_empty() {
            for x in remote_txn.log(&*rem, from)? {
                debug!("log {:?}", x);
                let (n, (h, m)) = x?;
                assert!(n >= from);
                debug!("put_remote {:?} {:?} {:?}", n, h, m);
                if tags.get(tagsi) == Some(&n) {
                    f(a, n, h.into(), m.into(), true)?;
                    tagsi += 1;
                } else {
                    f(a, n, h.into(), m.into(), false)?;
                }
            }
        } else {
            let mut hashes = HashMap::new();
            let mut stack = Vec::new();
            for x in remote_txn.log(&*rem, from)? {
                debug!("log {:?}", x);
                let (n, (h, m)) = x?;
                assert!(n >= from);
                let h_int = remote_txn.get_internal(h)?.unwrap();
                if paths_.is_empty()
                    || paths_.iter().any(|x| {
                        let y = remote_txn.get_touched_files(x, Some(h_int)).unwrap();
                        debug!("x {:?} {:?}", x, y);
                        y == Some(h_int)
                    })
                {
                    stack.push((*h_int, *m, n));
                }
            }

            while let Some((h_int, m, n)) = stack.pop() {
                if hashes.insert(h_int, (m, n)).is_some() {
                    continue;
                }
                for d in remote_txn.iter_dep(&h_int)? {
                    let (&h_int_, &d) = d?;
                    if h_int_ < h_int {
                        continue;
                    } else if h_int_ > h_int {
                        break;
                    }
                    let n = remote_txn
                        .get_changeset(remote_txn.changes(&*rem), &d)
                        .unwrap()
                        .unwrap();
                    let m = remote_txn
                        .get_revchangeset(remote_txn.rev_changes(&*rem), &n)
                        .unwrap()
                        .unwrap()
                        .b;
                    stack.push((d, m.into(), (*n).into()))
                }
            }

            let mut hashes: Vec<_> = hashes.into_iter().collect();
            hashes.sort_by_key(|(_, (_, n))| *n);
            for (h_int, (m, n)) in hashes {
                let h = remote_txn.get_external(&h_int).optional()?.unwrap();
                debug!("put_remote {:?} {:?} {:?}", n, h, m);
                if tags.get(tagsi) == Some(&n) {
                    f(a, n, h.into(), m.into(), true)?;
                    tagsi += 1;
                } else {
                    f(a, n, h.into(), m.into(), false)?;
                }
            }
        }
        Ok(result)
    }

    pub fn upload_changes(
        &mut self,
        progress_bar: ProgressBar,
        mut local: PathBuf,
        to_channel: Option<&str>,
        changes: &[CS],
    ) -> Result<(), RemoteError> {
        let store = pijul_core::changestore::filesystem::FileSystem::from_root(
            &self.root,
            pijul_repository::max_files(),
        );
        let txn = self.pristine.arc_txn_begin()?;
        let channel_name: pijul_core::small_string::SmallString =
            to_channel.unwrap_or(&self.channel).parse()?;
        let channel = txn.write().open_or_create_channel(&channel_name)?;
        for c in changes {
            match c {
                CS::Change(c) => {
                    pijul_core::changestore::filesystem::push_filename(&mut local, &c);
                    pijul_core::changestore::filesystem::push_filename(&mut self.changes_dir, &c);
                }
                CS::State(c) => {
                    pijul_core::changestore::filesystem::push_tag_filename(&mut local, &c);
                    pijul_core::changestore::filesystem::push_tag_filename(
                        &mut self.changes_dir,
                        &c,
                    );
                }
            }
            let parent = self.changes_dir.parent().ok_or(RemoteError::NoPathParent)?;
            std::fs::create_dir_all(parent).map_err(RemoteError::Io)?;
            debug!("hard link {:?} {:?}", local, self.changes_dir);
            if std::fs::metadata(&self.changes_dir).is_err() {
                if std::fs::hard_link(&local, &self.changes_dir).is_err() {
                    std::fs::copy(&local, &self.changes_dir).map_err(RemoteError::Io)?;
                }
            }
            debug!("hard link done");
            pijul_core::changestore::filesystem::pop_filename(&mut local);
            pijul_core::changestore::filesystem::pop_filename(&mut self.changes_dir);
        }
        let repo = pijul_core::working_copy::filesystem::FileSystem::from_root(&self.root);

        // Protect the remote's pending patch (its unrecorded working-copy
        // changes): record it *before* applying the incoming changes so the
        // `output` below merges them with the pending patch (conflict markers if
        // needed) instead of overwriting it, then unrecord it afterwards. This
        // mirrors `pijul pull`, which does the same for the puller's working
        // copy; without it a push silently clobbers unrecorded work in the
        // remote's working copy (`output_repository_no_pending` cancels any
        // unrecorded change).
        let pending = crate::pending(txn.clone(), &channel, &repo, &store)?;

        {
            let mut ws = pijul_core::ApplyWorkspace::new();
            let mut ch = channel.write();
            let mut txn_ = txn.write();
            for c in changes {
                match c {
                    CS::Change(c) => {
                        txn_.apply_change_ws(&store, &mut *ch, c, &mut ws)
                            .map_err(RemoteError::Apply)?;
                    }
                    CS::State(c) => {
                        if let Some(n) = txn_.channel_has_state(txn_.states(&*ch), &c.into())? {
                            let tags = txn_.tags_mut(&mut *ch);
                            txn_.put_tags(tags, n.into(), c)
                                .map_err(RemoteError::LocalTxn)?;
                        } else {
                            return Err((RemoteError::TagNotInChannel { tag: *c }).into());
                        }
                    }
                }
                progress_bar.inc(1);
            }
        }
        // Output only the files touched by the applied changes. Rewriting the
        // whole working copy (prefix "", `if_modified_since` None) bumps every
        // file's mtime, which invalidates `record`'s stat cache and makes the
        // remote's next `pijul record` re-diff the entire tree. Mirrors the
        // touched-files logic of `pijul pull` (see pushpull.rs).
        let mut touched = HashSet::new();
        {
            let txn_ = txn.read();
            for c in changes {
                let h = match c {
                    CS::Change(h) => h,
                    CS::State(_) => continue,
                };
                if let Some(int) = txn_.get_internal(&h.into())? {
                    for inode in txn_.iter_rev_touched(int)? {
                        let (int_, inode) = inode?;
                        if int_ < int {
                            continue;
                        } else if int_ > int {
                            break;
                        }
                        touched.insert(*inode);
                    }
                }
            }
        }
        let mut touched_paths = std::collections::BTreeSet::new();
        {
            let txn_ = txn.read();
            let channel_ = channel.read();
            for i in touched {
                match pijul_core::fs::find_path(&store, &*txn_, &*channel_, false, i)
                    .map_err(find_path_err)?
                {
                    Some(pijul_core::fs::FindPath { path, .. }) => {
                        touched_paths.insert(path.join("/"));
                    }
                    None => {
                        // Path unresolved: fall back to a full re-output.
                        touched_paths.clear();
                        touched_paths.insert(String::new());
                        break;
                    }
                }
            }
        }
        let mut last: Option<String> = None;
        for path in touched_paths {
            if let Some(last_path) = &last {
                // Skip paths already covered by a previous prefix output.
                if last_path.len() < path.len() {
                    let (pre, post) = path.split_at(last_path.len());
                    if pre == last_path.as_str() && post.starts_with('/') {
                        continue;
                    }
                }
            }
            pijul_core::output::output_repository_no_pending(
                &repo,
                &store,
                &txn,
                &channel,
                &path,
                true,
                None,
                std::thread::available_parallelism()?.get(),
                0,
            )
            .map_err(output_err)?;
            last = Some(path);
        }

        // Restore the remote's pending patch to unrecorded state: unrecord it
        // (the working copy keeps the merged content output above) and drop its
        // ephemeral change file.
        if let Some(h) = pending {
            use pijul_core::changestore::ChangeStore;
            let mut touched = pijul_core::unrecord::TouchedInodes::new();
            txn.write()
                .unrecord(&store, &channel, &h, 0, &mut touched)
                .map_err(|e| RemoteError::RemoteError(e.to_string()))?;
            store.del_change(&h)?;
        }

        txn.commit()?;
        Ok(())
    }

    pub async fn download_changes(
        &mut self,
        progress_bar: ProgressBar,
        hashes: &mut tokio::sync::mpsc::UnboundedReceiver<CS>,
        send: &mut tokio::sync::mpsc::Sender<(CS, bool)>,
        mut path: &mut PathBuf,
    ) -> Result<(), RemoteError> {
        while let Some(c) = hashes.recv().await {
            match c {
                CS::Change(c) => {
                    pijul_core::changestore::filesystem::push_filename(&mut self.changes_dir, &c);
                    pijul_core::changestore::filesystem::push_filename(&mut path, &c);
                }
                CS::State(c) => {
                    pijul_core::changestore::filesystem::push_tag_filename(
                        &mut self.changes_dir,
                        &c,
                    );
                    pijul_core::changestore::filesystem::push_tag_filename(&mut path, &c);
                }
            }
            progress_bar.inc(1);

            if std::fs::metadata(&path).is_ok() {
                debug!("metadata {:?} ok", path);
                pijul_core::changestore::filesystem::pop_filename(&mut self.changes_dir);
                pijul_core::changestore::filesystem::pop_filename(&mut path);
                send.send((c, true))
                    .await
                    .map_err(|_| RemoteError::ChannelClosed)?;
                continue;
            }
            let parent = path.parent().ok_or(RemoteError::NoPathParent)?;
            std::fs::create_dir_all(parent)?;
            if std::fs::hard_link(&self.changes_dir, &path).is_err() {
                std::fs::copy(&self.changes_dir, &path)?;
            }
            pijul_core::changestore::filesystem::pop_filename(&mut self.changes_dir);
            pijul_core::changestore::filesystem::pop_filename(&mut path);
            send.send((c, true))
                .await
                .map_err(|_| RemoteError::ChannelClosed)?;
        }
        Ok(())
    }

    pub async fn update_identities(
        &mut self,
        _rev: Option<u64>,
        mut path: PathBuf,
    ) -> Result<u64, RemoteError> {
        let mut other_path = self.root.join(DOT_DIR);
        other_path.push("identities");
        let r = if let Ok(r) = std::fs::read_dir(&other_path) {
            r
        } else {
            return Ok(0);
        };
        std::fs::create_dir_all(&path)?;
        for id in r {
            let id = id?;
            let m = id.metadata()?;
            let p = id.path();
            path.push(p.file_name().unwrap());
            if let Ok(ml) = std::fs::metadata(&path) {
                if ml.modified()? < m.modified()? {
                    std::fs::remove_file(&path)?;
                } else {
                    path.pop();
                    continue;
                }
            }
            if std::fs::hard_link(&p, &path).is_err() {
                std::fs::copy(&p, &path)?;
            }
            debug!("hard link done");
            path.pop();
        }
        Ok(0)
    }
}

pub type LocalTxn =
    pijul_core::pristine::sanakirja::GenericTxn<sanakirja::MutTxn<Arc<sanakirja::Env>>>;

/// Map an `output_repository_no_pending` error to a `RemoteError`.
fn output_err(
    e: pijul_core::output::OutputError<
        pijul_core::changestore::filesystem::Error,
        LocalTxn,
        std::io::Error,
    >,
) -> RemoteError {
    match e {
        pijul_core::output::OutputError::WorkingCopy(e) => RemoteError::Io(e),
        pijul_core::output::OutputError::SmallString(e) => RemoteError::SmallString(e),
        pijul_core::output::OutputError::Pristine(e) => match e {
            pijul_core::output::PristineOutputError::Channel(e) => RemoteError::TxnSanakirja(e),
            pijul_core::output::PristineOutputError::Tree(e) => RemoteError::TreeSanakirja(e),
            pijul_core::output::PristineOutputError::Changestore(e) => RemoteError::Filesystem(e),
            pijul_core::output::PristineOutputError::Io(e) => RemoteError::Io(e),
            pijul_core::output::PristineOutputError::Fs(e) => match e {
                pijul_core::FsError::Tree(e) => RemoteError::TreeSanakirja(e),
                pijul_core::FsError::SmallString(e) => RemoteError::SmallString(e),
                _ => RemoteError::Corruption,
            },
        },
    }
}

/// Map a `find_path` error to a `RemoteError`.
fn find_path_err(
    e: pijul_core::output::FileError<pijul_core::changestore::filesystem::Error, LocalTxn>,
) -> RemoteError {
    match e {
        pijul_core::output::FileError::Changestore(e) => RemoteError::Filesystem(e),
        pijul_core::output::FileError::Txn(e) => RemoteError::TxnSanakirja(e),
        pijul_core::output::FileError::Io(e) => RemoteError::Io(e),
    }
}
/*
pub enum LocalError {
    Remote(crate::error::RemoteError),
    Sanakirja(pijul_core::pristine::sanakirja::SanakirjaError),
    SmallStr(pijul_core::small_string::Error),
    Apply(pijul_core::ApplyError<pijul_core::changestore::filesystem::Error, LocalTxn>),
}

impl<T: GraphTxnT + TreeTxnT> From<LocalError> for crate::error::Error<T> {
    fn from(e: LocalError) -> Self {
        match e {
            LocalError::Remote(r) => crate::error::Error::Remote(r),
            LocalError::Sanakirja(r) => crate::error::Error::Sanakirja(r),
            LocalError::SmallStr(r) => crate::error::Error::SmallString(r),
        }
    }
}
*/
pub fn upload_changes<T: MutTxnTExt>(
    progress_bar: ProgressBar,
    store: &pijul_core::changestore::filesystem::FileSystem,
    txn: &mut T,
    channel: &pijul_core::pristine::ChannelRef<T>,
    changes: &[CS],
) -> Result<(), crate::error::Error<T>> {
    let mut ws = pijul_core::ApplyWorkspace::new();
    let mut channel = channel.write();
    for c in changes {
        match c {
            CS::Change(c) => {
                txn.apply_change_ws(store, &mut *channel, c, &mut ws)
                    .map_err(crate::error::Error::Apply)?;
            }
            CS::State(c) => {
                if let Some(n) = txn
                    .channel_has_state(txn.states(&*channel), &c.into())
                    .map_err(crate::error::Error::TxnErrGraph)?
                {
                    let tags = txn.tags_mut(&mut *channel);
                    txn.put_tags(tags, n.into(), c)
                        .map_err(crate::error::Error::TxnErrGraph)?;
                } else {
                    return Err(RemoteError::TagNotInChannel { tag: *c }.into());
                }
            }
        }
        progress_bar.inc(1);
    }
    Ok(())
}
