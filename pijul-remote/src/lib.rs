use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use log::{debug, info};
use pijul_core::DOT_DIR;
use pijul_core::pristine::{
    ArcTxn, Base32, ChangeId, ChangePosition, ChannelRef, GraphIter, Hash, Merkle, MutTxnT,
    Position, RemoteRef, TxnT, sanakirja::MutTxn, sanakirja::RawMutTxnT,
};
use pijul_core::{ChannelTxnT, DepsTxnT, GraphTxnT, MutTxnTExt, TreeTxnT, TxnTExt};
use regex::Regex;

use pijul_config::remote::{RemoteConfig, RemoteHttpHeader};
use pijul_identity::Complete;
use pijul_repository::*;

pub mod error;
pub use error::RemoteError;

pub mod ssh;
use ssh::*;

pub mod ssh_config;

pub mod local;
use local::*;

pub mod http;
use http::*;

use pijul_interaction::{
    APPLY_MESSAGE, COMPLETE_MESSAGE, DOWNLOAD_MESSAGE, ProgressBar, Spinner, UPLOAD_MESSAGE,
};

pub const PROTOCOL_VERSION: usize = 3;

/// Record the working copy's pending (unrecorded) changes as an ephemeral patch
/// on `channel`, returning its hash — or `None` if the working copy is clean.
///
/// Shared by every operation that must apply changes on top of an unrecorded
/// working copy without destroying it: `pull` and `unrecord --reset` (the local
/// side), and a push into a local repository (`Local::upload_changes`, the
/// remote side). The caller records this patch, applies/outputs the incoming
/// changes on top so `output` merges them with it instead of overwriting, then
/// unrecords it to restore the working copy's changes to their unrecorded state.
pub fn pending<T: MutTxnTExt + TxnT + Send + Sync + 'static>(
    txn: ArcTxn<T>,
    channel: &ChannelRef<T>,
    working_copy: &pijul_core::working_copy::filesystem::FileSystem,
    changes: &pijul_core::changestore::filesystem::FileSystem,
) -> Result<Option<Hash>, RemoteError> {
    use pijul_core::changestore::ChangeStore;

    let mut builder = pijul_core::record::Builder::new();
    builder
        .record(
            txn.clone(),
            pijul_core::Algorithm::default(),
            false,
            &pijul_core::DEFAULT_SEPARATOR,
            channel.clone(),
            working_copy,
            changes,
            "",
            std::thread::available_parallelism()?.get(),
        )
        .map_err(|e| RemoteError::RemoteError(e.to_string()))?;
    let recorded = builder.finish();
    if recorded.actions.is_empty() {
        return Ok(None);
    }
    let mut txn = txn.write();
    let actions = recorded
        .actions
        .into_iter()
        .map(|rec| rec.globalize(&*txn).unwrap())
        .collect();
    let contents = if let Ok(c) = Arc::try_unwrap(recorded.contents) {
        c.into_inner()
    } else {
        unreachable!()
    };
    let mut pending_change = pijul_core::change::Change::make_change(
        &*txn,
        channel,
        actions,
        contents,
        pijul_core::change::ChangeHeader::default(),
        Vec::new(),
    )
    .map_err(|e| RemoteError::RemoteError(e.to_string()))?;
    let (dependencies, extra_known) =
        pijul_core::change::dependencies(&*txn, &*channel.read(), pending_change.changes.iter())
            .map_err(|e| RemoteError::RemoteError(e.to_string()))?;
    pending_change.dependencies = dependencies;
    pending_change.extra_known = extra_known;
    let hash = changes.save_change(&mut pending_change, |_, _| Ok::<_, RemoteError>(()))?;
    txn.apply_local_change(channel, &pending_change, &hash, &recorded.updatables)
        .map_err(|e| RemoteError::RemoteError(e.to_string()))?;
    Ok(Some(hash))
}

pub enum RemoteRepo {
    Local(Local),
    Ssh(Ssh),
    Http(Http),
    LocalChannel(String),
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CS {
    Change(Hash),
    State(Merkle),
}

pub async fn repository(
    config: &pijul_config::Config,
    self_path: Option<&Path>,
    user: Option<&str>,
    name: &str,
    channel: &str,
    no_cert_check: bool,
    with_path: bool,
) -> Result<RemoteRepo, RemoteError> {
    if let Some(name) = config.remotes.iter().find(|e| e.name() == name) {
        name.to_remote(config, channel, no_cert_check, with_path)
            .await
    } else {
        unknown_remote(
            config,
            self_path,
            user,
            name,
            channel,
            no_cert_check,
            with_path,
        )
        .await
    }
}

/// Associate a generated key with a remote identity.
pub async fn prove(
    config: &pijul_config::Config,
    identity: &Complete,
    origin: Option<&str>,
    no_cert_check: bool,
) -> Result<(), RemoteError> {
    let remote = origin.unwrap_or(&identity.config.author.origin);
    let mut stderr = std::io::stderr();
    writeln!(
        stderr,
        "Linking identity `{}` with {}@{}",
        &identity.name, &identity.config.author.username, remote
    )?;

    let mut remote = repository(
        config,
        None,
        Some(&identity.config.author.username),
        &remote,
        pijul_core::DEFAULT_CHANNEL,
        no_cert_check,
        false,
    )
    .await?;

    remote.prove(identity.skey()).await?;

    Ok(())
}

fn shell_cmd(s: &str) -> Result<String, RemoteError> {
    let out = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(&["/C", s])
            .output()
            .expect("failed to execute process")
    } else {
        std::process::Command::new(std::env::var("SHELL").unwrap_or("sh".to_string()))
            .arg("-c")
            .arg(s)
            .output()
            .expect("failed to execute process")
    };
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

#[allow(async_fn_in_trait)]
pub trait ToRemote {
    async fn to_remote(
        &self,
        config: &pijul_config::Config,
        channel: &str,
        no_cert_check: bool,
        with_path: bool,
    ) -> Result<RemoteRepo, RemoteError>;
}

impl ToRemote for RemoteConfig {
    async fn to_remote(
        &self,
        config: &pijul_config::Config,
        channel: &str,
        no_cert_check: bool,
        with_path: bool,
    ) -> Result<RemoteRepo, RemoteError> {
        match self {
            RemoteConfig::Ssh { ssh, .. } => {
                if let Some(mut sshr) = ssh_remote(None, ssh, with_path) {
                    debug!("unknown_remote, ssh = {:?}", ssh);
                    if let Some(c) = sshr.connect(config, ssh, channel).await? {
                        return Ok(RemoteRepo::Ssh(c));
                    }
                }
                Err(RemoteError::RemoteNotFound(format!("{:?}", ssh)))
            }
            RemoteConfig::Http {
                http,
                headers,
                name,
            } => {
                let mut h = Vec::new();
                for (k, v) in headers.iter() {
                    match v {
                        RemoteHttpHeader::String(s) => {
                            h.push((k.clone(), s.clone()));
                        }
                        RemoteHttpHeader::Shell(shell) => {
                            h.push((k.clone(), shell_cmd(&shell.shell)?));
                        }
                    }
                }
                Ok(RemoteRepo::Http(Http {
                    url: http.parse()?,
                    channel: channel.to_string(),
                    client: reqwest::ClientBuilder::new()
                        .danger_accept_invalid_certs(no_cert_check)
                        .build()?,
                    headers: h,
                    name: name.to_string(),
                }))
            }
        }
    }
}

pub async fn unknown_remote(
    config: &pijul_config::Config,
    self_path: Option<&Path>,
    user: Option<&str>,
    name: &str,
    channel: &str,
    no_cert_check: bool,
    with_path: bool,
) -> Result<RemoteRepo, RemoteError> {
    if let Ok(url) = url::Url::parse(name) {
        let scheme = url.scheme();
        if scheme == "http" || scheme == "https" {
            debug!("unknown_remote, http = {:?}", name);
            return Ok(RemoteRepo::Http(Http {
                url,
                channel: channel.to_string(),
                client: reqwest::ClientBuilder::new()
                    .danger_accept_invalid_certs(no_cert_check)
                    .build()?,
                headers: Vec::new(),
                name: name.to_string(),
            }));
        } else if scheme == "ssh" {
            if let Some(mut ssh) = ssh_remote(user, name, with_path) {
                debug!("unknown_remote, ssh = {:?}", ssh);
                if let Some(c) = ssh.connect(config, name, channel).await? {
                    return Ok(RemoteRepo::Ssh(c));
                }
            }
            return Err(RemoteError::RemoteNotFound(format!("{:?}", name)));
        } else {
            return Err(RemoteError::UnsupportedScheme(format!("{:?}", scheme)));
        }
    }
    if let Ok(root) = std::fs::canonicalize(name) {
        if let Some(path) = self_path {
            let path = std::fs::canonicalize(path)?;
            if path == root {
                return Ok(RemoteRepo::LocalChannel(channel.to_string()));
            }
        }

        let mut dot_dir = root.join(DOT_DIR);
        let changes_dir = dot_dir.join(CHANGES_DIR);

        dot_dir.push(PRISTINE_DIR);
        debug!("dot_dir = {:?}", dot_dir);
        match pijul_core::pristine::sanakirja::Pristine::new(&dot_dir.join("db")) {
            Ok(pristine) => {
                debug!("pristine done");
                return Ok(RemoteRepo::Local(Local {
                    root: Path::new(name).to_path_buf(),
                    channel: channel.to_string(),
                    changes_dir,
                    pristine: Arc::new(pristine),
                    name: name.to_string(),
                }));
            }
            Err(pijul_core::pristine::sanakirja::SanakirjaError::Sanakirja(
                sanakirja::Error::IO(e),
            )) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!("repo not found")
            }
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(mut ssh) = ssh_remote(user, name, with_path) {
        debug!("unknown_remote, ssh = {:?}", ssh);
        if let Some(c) = ssh.connect(config, name, channel).await? {
            return Ok(RemoteRepo::Ssh(c));
        }
    }
    Err(RemoteError::RemoteNotFound(format!("{:?}", name)))
}

pub fn get_local_inodes<T>(
    txn: &mut T,
    channel: &ChannelRef<T>,
    repo: &Repository,
    path: &[String],
) -> Result<HashSet<Position<ChangeId>>, error::Error<T>>
where
    T: TxnTExt + ChannelTxnT + GraphTxnT + TreeTxnT + DepsTxnT,
    T::GraphError: std::error::Error + Into<RemoteError> + 'static,
    T::TreeError: std::error::Error + Into<RemoteError> + 'static,
{
    let mut paths = HashSet::new();
    for path in path.iter() {
        let (p, ambiguous) = txn.follow_oldest_path(&repo.changes, channel, path)?;

        if ambiguous {
            return Err(RemoteError::AmbiguousPath(format!("{:?}", path)).into());
        }
        paths.insert(p);
        {
            let ch = channel.read();
            paths.extend(
                pijul_core::fs::iter_graph_descendants(txn, txn.graph(&*ch), p)
                    .map_err(|e| e.into())?
                    .map(|x| x.unwrap()),
            );
        }
    }
    Ok(paths)
}

pub struct PushDelta {
    pub to_upload: Vec<CS>,
    pub remote_unrecs: Vec<(u64, CS)>,
    pub unknown_changes: Vec<CS>,
    /// The remote channel's current head Merkle, taken from the local remote
    /// cache that the changelist exchange just refreshed — so push signatures
    /// can commit to the applied-onto state without an extra round-trip.
    pub remote_state: Option<Merkle>,
}

pub struct RemoteDelta<T: MutTxnT> {
    pub inodes: HashSet<Position<Hash>>,
    pub remote_ref: Option<RemoteRef<T>>,
    pub to_download: Vec<CS>,
    pub ours_ge_dichotomy_set: HashSet<CS>,
    pub theirs_ge_dichotomy: Vec<(u64, Hash, Merkle, bool)>,
    pub theirs_ge_dichotomy_set: HashSet<CS>,
    pub remote_unrecs: Vec<(u64, CS)>,
}

impl<T: MutTxnT + TxnTExt + ChannelTxnT + DepsTxnT + GraphTxnT + TreeTxnT> RemoteDelta<T>
where
    RemoteError: From<pijul_core::pristine::TxnErr<T::GraphError>>,
    T::GraphError: std::error::Error + Into<RemoteError> + 'static,
    T::TreeError: std::error::Error + Into<RemoteError> + 'static,
{
    pub fn to_local_channel_push(
        self,
        txn: &ArcTxn<T>,
        remote_channel: &str,
        path: &[String],
        channel: &ChannelRef<T>,
        repo: &Repository,
    ) -> Result<PushDelta, error::Error<T>> {
        let mut to_upload = Vec::new();
        let mut txn = txn.write();
        let inodes = get_local_inodes(&mut *txn, channel, repo, path)?;

        for x in txn
            .reverse_log(&*channel.read(), None)
            .map_err(crate::error::Error::Graph)?
        {
            let (_, (h, _)) = x.map_err(crate::error::Error::Graph)?;
            if let Some(channel) = txn
                .load_channel(remote_channel.parse()?)
                .map_err(crate::error::Error::TxnErrGraph)?
            {
                let channel = channel.read();
                let h_int = txn
                    .get_internal(h)
                    .map_err(crate::error::Error::TxnErrGraph)?
                    .unwrap();
                if txn
                    .get_changeset(txn.changes(&channel), h_int)
                    .map_err(crate::error::Error::TxnErrGraph)?
                    .is_none()
                {
                    if inodes.is_empty() {
                        to_upload.push(CS::Change(h.into()))
                    } else {
                        for p in inodes.iter() {
                            if txn
                                .get_touched_files(p, Some(h_int))
                                .map_err(crate::error::Error::TxnErrGraph)?
                                .is_some()
                            {
                                to_upload.push(CS::Change(h.into()));
                                break;
                            }
                        }
                    }
                }
            }
        }
        assert!(self.ours_ge_dichotomy_set.is_empty());
        assert!(self.theirs_ge_dichotomy_set.is_empty());
        let d = PushDelta {
            to_upload: to_upload.into_iter().rev().collect(),
            remote_unrecs: self.remote_unrecs,
            unknown_changes: Vec::new(),
            // Local-channel push: no remote Nest, so no push signatures.
            remote_state: None,
        };
        assert!(d.remote_unrecs.is_empty());
        Ok(d)
    }

    pub fn to_remote_push(
        self,
        txn: &mut T,
        path: &[String],
        channel: &ChannelRef<T>,
        repo: &Repository,
    ) -> Result<PushDelta, error::Error<T>> {
        debug!("to_remote_push {}", self.remote_ref.is_some());
        let mut to_upload = Vec::new();
        let inodes = get_local_inodes(txn, channel, repo, path)?;
        let mut tags: HashSet<Merkle> = HashSet::new();
        if let Some(ref remote_ref) = self.remote_ref {
            for x in txn
                .rev_iter_tags(txn.tags(&*channel.read()), None)
                .map_err(crate::error::Error::TxnErrGraph)?
            {
                let (n, m) = x.map_err(crate::error::Error::TxnErrGraph)?;
                debug!("rev_iter_tags {:?} {:?}", n, m);
                if let Some((_, p)) = txn
                    .get_remote_tag(&remote_ref.lock().tags, (*n).into())
                    .map_err(crate::error::Error::TxnErrGraph)?
                {
                    if p.b == m.b {
                        debug!("the remote has tag {:?}", p.a);
                        break;
                    }
                    if p.a != m.a {
                        // state `n` may differ between local and remote
                    }
                } else {
                    tags.insert(m.a.into());
                }
            }
            debug!("tags = {:?}", tags);
            for x in txn
                .reverse_log(&*channel.read(), None)
                .map_err(crate::error::Error::Graph)?
            {
                let (_, (h, m)) = x.map_err(crate::error::Error::Graph)?;
                let h_unrecorded = self
                    .remote_unrecs
                    .iter()
                    .any(|(_, hh)| hh == &CS::Change(h.into()));
                if !h_unrecorded {
                    if txn
                        .remote_has_state(remote_ref, &m)
                        .map_err(crate::error::Error::TxnErrGraph)?
                        .is_some()
                    {
                        debug!("remote_has_state: {:?}", m);
                        break;
                    }
                }
                let h_int = txn
                    .get_internal(h)
                    .map_err(crate::error::Error::TxnErrGraph)?
                    .unwrap();
                let h_deser = Hash::from(h);
                if (!txn
                    .remote_has_change(remote_ref, &h)
                    .map_err(crate::error::Error::TxnErrGraph)?
                    || h_unrecorded)
                    && !self.theirs_ge_dichotomy_set.contains(&CS::Change(h_deser))
                {
                    if inodes.is_empty() {
                        if tags.remove(&m.into()) {
                            to_upload.push(CS::State(m.into()));
                        }
                        to_upload.push(CS::Change(h_deser));
                    } else {
                        for p in inodes.iter() {
                            if txn
                                .get_touched_files(p, Some(h_int))
                                .map_err(crate::error::Error::TxnErrGraph)?
                                .is_some()
                            {
                                to_upload.push(CS::Change(h_deser));
                                if tags.remove(&m.into()) {
                                    to_upload.push(CS::State(m.into()));
                                }
                                break;
                            }
                        }
                    }
                }
            }
            for t in tags.iter() {
                if let Some(n) = txn
                    .remote_has_state(&remote_ref, &t.into())
                    .map_err(crate::error::Error::TxnErrGraph)?
                {
                    if !txn
                        .is_tagged(&remote_ref.lock().tags, n)
                        .map_err(crate::error::Error::TxnErrGraph)?
                    {
                        to_upload.push(CS::State(*t));
                    }
                } else {
                    debug!("the remote doesn't have state {:?}", t);
                }
            }
        } else {
            for x in txn
                .reverse_log(&*channel.read(), None)
                .map_err(crate::error::Error::Graph)?
            {
                let (_, (h, m)) = x.map_err(crate::error::Error::Graph)?;
                let h_int = txn
                    .get_internal(h)
                    .map_err(crate::error::Error::TxnErrGraph)?
                    .unwrap();
                let h_deser = Hash::from(h);
                if inodes.is_empty() {
                    if tags.remove(&m.into()) {
                        to_upload.push(CS::State(m.into()));
                    }
                    to_upload.push(CS::Change(h_deser));
                } else {
                    for p in inodes.iter() {
                        if txn
                            .get_touched_files(p, Some(h_int))
                            .map_err(crate::error::Error::TxnErrGraph)?
                            .is_some()
                        {
                            to_upload.push(CS::Change(h_deser));
                            if tags.remove(&m.into()) {
                                to_upload.push(CS::State(m.into()));
                            }
                            break;
                        }
                    }
                }
            }
        }

        let mut unknown_changes = Vec::new();
        for (_, h, m, is_tag) in self.theirs_ge_dichotomy.iter() {
            let h_is_known = txn
                .get_revchanges(&channel, h)
                .map_err(crate::error::Error::Graph)?
                .is_some();
            let change = CS::Change(*h);
            if !(self.ours_ge_dichotomy_set.contains(&change) || h_is_known) {
                unknown_changes.push(change)
            }
            if *is_tag {
                let m_is_known = if let Some(n) = txn
                    .channel_has_state(txn.states(&*channel.read()), &m.into())
                    .map_err(crate::error::Error::TxnErrGraph)?
                {
                    txn.is_tagged(txn.tags(&*channel.read()), n.into())
                        .map_err(crate::error::Error::TxnErrGraph)?
                } else {
                    false
                };
                if !m_is_known {
                    unknown_changes.push(CS::State(*m))
                }
            }
        }

        // The remote's head, straight from the just-synced remote cache — no
        // network round-trip (which would otherwise stall the push).
        let remote_state = if let Some(ref remote_ref) = self.remote_ref {
            txn.last_remote(&remote_ref.lock().remote)
                .map_err(crate::error::Error::TxnErrGraph)?
                .map(|(_, v)| (&v.b).into())
        } else {
            None
        };

        Ok(PushDelta {
            to_upload: to_upload.into_iter().rev().collect(),
            remote_unrecs: self.remote_unrecs,
            unknown_changes,
            remote_state,
        })
    }
}

pub fn update_changelist_local_channel<T: RawMutTxnT + 'static>(
    remote_channel: &str,
    txn: &mut MutTxn<T>,
    path: &[String],
    current_channel: &ChannelRef<MutTxn<T>>,
    repo: &Repository,
    specific_changes: &[String],
) -> Result<RemoteDelta<MutTxn<T>>, error::Error<pijul_core::pristine::sanakirja::GenericTxn<T>>> {
    if !specific_changes.is_empty() {
        let mut to_download = Vec::new();
        for h in specific_changes {
            let h = txn
                .hash_from_prefix(h)
                .map_err(crate::error::RemoteError::HashPrefix)?
                .0;
            if txn.get_revchanges(current_channel, &h)?.is_none() {
                to_download.push(CS::Change(h));
            }
        }
        Ok(RemoteDelta {
            inodes: HashSet::new(),
            to_download,
            remote_ref: None,
            ours_ge_dichotomy_set: HashSet::new(),
            theirs_ge_dichotomy: Vec::new(),
            theirs_ge_dichotomy_set: HashSet::new(),
            remote_unrecs: Vec::new(),
        })
    } else {
        let mut inodes = HashSet::new();
        let inodes_ = get_local_inodes(txn, current_channel, repo, path)?;
        let mut to_download = Vec::new();
        inodes.extend(inodes_.iter().map(|x| pijul_core::pristine::Position {
            change: txn.get_external(&x.change).unwrap().into(),
            pos: x.pos,
        }));
        if let Some(remote_channel) = txn.load_channel(remote_channel.parse()?)? {
            let remote_channel = remote_channel.read();
            for x in txn.reverse_log(&remote_channel, None)? {
                let (_, (h, m)) = x?;
                if txn
                    .channel_has_state(txn.states(&*current_channel.read()), &m)?
                    .is_some()
                {
                    break;
                }
                let h_int = txn.get_internal(h)?.unwrap();
                if txn
                    .get_changeset(txn.changes(&*current_channel.read()), h_int)?
                    .is_none()
                {
                    if inodes_.is_empty()
                        || inodes_.iter().any(|&inode| {
                            txn.get_rev_touched_files(h_int, Some(&inode))
                                .unwrap()
                                .is_some()
                        })
                    {
                        to_download.push(CS::Change(h.into()));
                    }
                }
            }
        }
        Ok(RemoteDelta {
            inodes,
            to_download,
            remote_ref: None,
            ours_ge_dichotomy_set: HashSet::new(),
            theirs_ge_dichotomy: Vec::new(),
            theirs_ge_dichotomy_set: HashSet::new(),
            remote_unrecs: Vec::new(),
        })
    }
}

impl RemoteRepo {
    fn name(&self) -> Option<&str> {
        match *self {
            RemoteRepo::Ssh(ref s) => Some(s.name.as_str()),
            RemoteRepo::Local(ref l) => Some(l.name.as_str()),
            RemoteRepo::Http(ref h) => Some(h.name.as_str()),
            RemoteRepo::LocalChannel(_) => None,
            RemoteRepo::None => unreachable!(),
        }
    }

    pub fn repo_name(&self) -> Result<Option<String>, RemoteError> {
        match *self {
            RemoteRepo::Ssh(ref s) => {
                if let Some(sep) = s.name.rfind(|c| c == ':' || c == '/') {
                    Ok(Some(s.name.split_at(sep + 1).1.to_string()))
                } else {
                    Ok(Some(s.name.as_str().to_string()))
                }
            }
            RemoteRepo::Local(ref l) => {
                if let Some(file) = l.root.file_name() {
                    Ok(Some(
                        file.to_str()
                            .ok_or(RemoteError::InvalidRepositoryName)?
                            .to_string(),
                    ))
                } else {
                    Ok(None)
                }
            }
            RemoteRepo::Http(ref h) => {
                if let Some(name) = pijul_core::path::file_name(h.url.path()) {
                    if !name.trim().is_empty() {
                        return Ok(Some(name.trim().to_string()));
                    }
                }
                Ok(h.url.host().map(|h| h.to_string()))
            }
            RemoteRepo::LocalChannel(_) => Ok(None),
            RemoteRepo::None => unreachable!(),
        }
    }

    pub async fn finish(&mut self) -> Result<(), RemoteError> {
        if let RemoteRepo::Ssh(s) = self {
            s.finish().await?
        }
        Ok(())
    }

    pub async fn update_changelist<T: MutTxnTExt + TxnTExt + 'static>(
        &mut self,
        txn: &ArcTxn<T>,
        path: &[String],
    ) -> Result<Option<(HashSet<Position<Hash>>, RemoteRef<T>)>, error::Error<T>>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        debug!("update_changelist!");
        let id = if let Some(id) = self.get_id(txn).await? {
            id
        } else {
            return Ok(None);
        };
        debug!("id = {:?}", id);
        let mut remote = if let Some(name) = self.name() {
            let name_sm: pijul_core::small_string::SmallString = name.parse()?;
            txn.write()
                .open_or_create_remote(id, &name_sm)
                .map_err(error::Error::Graph)?
        } else {
            return Ok(None);
        };
        debug!("remote ok");
        let n = self.dichotomy_changelist(txn, &remote).await?;
        {
            debug!("update changelist {:?}", n);
            let v: Vec<_> = txn
                .read()
                .iter_remote(&remote.lock().remote, n)
                .map_err(error::Error::TxnErrGraph)?
                .filter_map(|k| {
                    debug!("filter_map {:?}", k);
                    let k = (*k.unwrap().0).into();
                    if k >= n { Some(k) } else { None }
                })
                .collect();
            for k in v {
                debug!("deleting {:?}", k);
                txn.write()
                    .del_remote(&mut remote, k)
                    .map_err(error::Error::TxnErrGraph)?;
            }
            let v: Vec<_> = txn
                .read()
                .iter_tags(&remote.lock().tags, n)
                .map_err(error::Error::TxnErrGraph)?
                .filter_map(|k| {
                    debug!("filter_map {:?}", k);
                    let k = (*k.unwrap().0).into();
                    if k >= n { Some(k) } else { None }
                })
                .collect();
            for k in v {
                debug!("deleting {:?}", k);
                txn.write()
                    .del_tags(&mut remote.lock().tags, k)
                    .map_err(error::Error::TxnErrGraph)?;
            }
        }
        debug!("deleted");
        let paths = self.download_changelist(txn, &mut remote, n, path).await?;
        Ok(Some((paths, remote)))
    }

    async fn update_changelist_pushpull_from_scratch<T: RawMutTxnT>(
        &mut self,
        txn: &ArcTxn<MutTxn<T>>,
        path: &[String],
        current_channel: &ChannelRef<MutTxn<T>>,
    ) -> Result<RemoteDelta<MutTxn<T>>, RemoteError> {
        debug!("no id, starting from scratch");
        let txn = txn.read();
        let (inodes, theirs_ge_dichotomy) = self.download_changelist_nocache(0, path).await?;
        let mut theirs_ge_dichotomy_set = HashSet::new();
        let mut to_download = Vec::new();
        for (_, h, m, is_tag) in theirs_ge_dichotomy.iter() {
            theirs_ge_dichotomy_set.insert(CS::Change(*h));
            if txn.get_revchanges(current_channel, h)?.is_none() {
                to_download.push(CS::Change(*h));
            }
            if *is_tag {
                let ch = current_channel.read();
                if let Some(n) = txn.channel_has_state(txn.states(&*ch), &m.into())? {
                    if !txn.is_tagged(txn.tags(&*ch), n.into())? {
                        to_download.push(CS::State(*m));
                    }
                } else {
                    to_download.push(CS::State(*m));
                }
            }
        }
        Ok(RemoteDelta {
            inodes,
            remote_ref: None,
            to_download,
            ours_ge_dichotomy_set: HashSet::new(),
            theirs_ge_dichotomy,
            theirs_ge_dichotomy_set,
            remote_unrecs: Vec::new(),
        })
    }

    pub async fn update_changelist_pushpull<T: RawMutTxnT + 'static>(
        &mut self,
        txn: &ArcTxn<MutTxn<T>>,
        path: &[String],
        current_channel: &ChannelRef<MutTxn<T>>,
        force_cache: Option<bool>,
        repo: &Repository,
        specific_changes: &[String],
        is_pull: bool,
    ) -> Result<RemoteDelta<MutTxn<T>>, error::Error<pijul_core::pristine::sanakirja::GenericTxn<T>>>
    {
        debug!("update_changelist_pushpull");
        if let RemoteRepo::LocalChannel(c) = self {
            return update_changelist_local_channel(
                c,
                &mut *txn.write(),
                path,
                current_channel,
                repo,
                specific_changes,
            );
        }

        let id = if let Some(id) = self.get_id(txn).await? {
            debug!("id = {:?}", id);
            id
        } else {
            return Ok(self
                .update_changelist_pushpull_from_scratch(txn, path, current_channel)
                .await?);
        };
        let name_sm: pijul_core::small_string::SmallString = self.name().unwrap().parse()?;
        let mut remote_ref = txn.write().open_or_create_remote(id, &name_sm)?;
        let dichotomy_n = self.dichotomy_changelist(txn, &remote_ref).await?;
        let ours_ge_dichotomy: Vec<(u64, CS)> = txn
            .read()
            .iter_remote(&remote_ref.lock().remote, dichotomy_n)?
            .filter_map(|k| {
                debug!("filter_map {:?}", k);
                match k.unwrap() {
                    (k, pijul_core::pristine::Pair { a: hash, .. }) => {
                        let (k, hash) = (u64::from(*k), Hash::from(*hash));
                        if k >= dichotomy_n {
                            Some((k, CS::Change(hash)))
                        } else {
                            None
                        }
                    }
                }
            })
            .collect();
        let (inodes, theirs_ge_dichotomy) =
            self.download_changelist_nocache(dichotomy_n, path).await?;
        debug!("theirs_ge_dichotomy = {:?}", theirs_ge_dichotomy);
        let ours_ge_dichotomy_set = ours_ge_dichotomy
            .iter()
            .map(|(_, h)| h)
            .copied()
            .collect::<HashSet<CS>>();
        let mut theirs_ge_dichotomy_set = HashSet::new();
        for (_, h, m, is_tag) in theirs_ge_dichotomy.iter() {
            theirs_ge_dichotomy_set.insert(CS::Change(*h));
            if *is_tag {
                theirs_ge_dichotomy_set.insert(CS::State(*m));
            }
        }

        let remote_unrecs = remote_unrecs(
            &*txn.read(),
            current_channel,
            &ours_ge_dichotomy,
            &theirs_ge_dichotomy_set,
        )?;
        debug!("should_cache = {:?} {:?}", force_cache, remote_unrecs);
        let mut txn = txn.write();

        use pijul_core::ChannelMutTxnT;
        for (k, t) in ours_ge_dichotomy.iter().copied() {
            match t {
                CS::State(_) => txn.del_tags(&mut remote_ref.lock().tags, k)?,
                CS::Change(_) => {
                    txn.del_remote(&mut remote_ref, k)?;
                }
            }
        }
        for (n, h, m, is_tag) in theirs_ge_dichotomy.iter().copied() {
            debug!("theirs: {:?} {:?} {:?}", n, h, m);
            txn.put_remote(&mut remote_ref, n, (h, m))?;
            if is_tag {
                txn.put_tags(&mut remote_ref.lock().tags, n, &m)?;
            }
        }

        if !specific_changes.is_empty() {
            let to_download = specific_changes
                .iter()
                .map(|h| {
                    if is_pull {
                        {
                            if let Ok(t) = txn.state_from_prefix(&remote_ref.lock().states, h) {
                                return Ok(CS::State(t.0));
                            }
                        }
                        Ok(CS::Change(txn.hash_from_prefix_remote(&remote_ref, h)?))
                    } else {
                        if let Ok(t) = txn.state_from_prefix(&current_channel.read().states, h) {
                            Ok(CS::State(t.0))
                        } else {
                            Ok(CS::Change(txn.hash_from_prefix(h)?.0))
                        }
                    }
                })
                .collect::<Result<Vec<_>, RemoteError>>();
            Ok(RemoteDelta {
                inodes,
                remote_ref: Some(remote_ref),
                to_download: to_download?,
                ours_ge_dichotomy_set,
                theirs_ge_dichotomy,
                theirs_ge_dichotomy_set,
                remote_unrecs,
            })
        } else {
            let mut to_download: Vec<CS> = Vec::new();
            let mut to_download_ = HashSet::new();
            for x in txn.iter_rev_remote(&remote_ref.lock().remote, None)? {
                let (_, p) = x?;
                let h: Hash = p.a.into();
                if txn
                    .channel_has_state(txn.states(&current_channel.read()), &p.b)?
                    .is_some()
                {
                    break;
                }
                if txn.get_revchanges(&current_channel, &h)?.is_none() {
                    let h = CS::Change(h);
                    if to_download_.insert(h.clone()) {
                        to_download.push(h);
                    }
                }
            }

            for (n, h, m, is_tag) in theirs_ge_dichotomy.iter() {
                debug!(
                    "update_changelist_pushpull line {}, {:?} {:?}",
                    line!(),
                    n,
                    h
                );
                let ch = CS::Change(*h);
                if txn.get_revchanges(&current_channel, h)?.is_none() {
                    if to_download_.insert(ch.clone()) {
                        to_download.push(ch.clone());
                    }
                    if *is_tag {
                        to_download.push(CS::State(*m));
                    }
                } else if *is_tag {
                    let has_tag = if let Some(n) =
                        txn.channel_has_state(txn.states(&current_channel.read()), &m.into())?
                    {
                        txn.is_tagged(txn.tags(&current_channel.read()), n.into())?
                    } else {
                        false
                    };
                    if !has_tag {
                        to_download.push(CS::State(*m));
                    }
                }
                if ours_ge_dichotomy_set.get(&ch).is_none() {
                    use pijul_core::ChannelMutTxnT;
                    txn.put_remote(&mut remote_ref, *n, (*h, *m))?;
                    if *is_tag {
                        let mut rem = remote_ref.lock();
                        txn.put_tags(&mut rem.tags, *n, m)?;
                    }
                }
            }
            Ok(RemoteDelta {
                inodes,
                remote_ref: Some(remote_ref),
                to_download,
                ours_ge_dichotomy_set,
                theirs_ge_dichotomy,
                theirs_ge_dichotomy_set,
                remote_unrecs,
            })
        }
    }

    pub async fn download_changelist_nocache(
        &mut self,
        from: u64,
        paths: &[String],
    ) -> Result<(HashSet<Position<Hash>>, Vec<(u64, Hash, Merkle, bool)>), RemoteError> {
        let mut v = Vec::new();
        let f = |v: &mut Vec<(u64, Hash, Merkle, bool)>, n, h, m, m2| {
            debug!("no cache: {:?}", h);
            Ok(v.push((n, h, m, m2)))
        };
        let r = match *self {
            RemoteRepo::Local(ref mut l) => l.download_changelist(f, &mut v, from, paths)?,
            RemoteRepo::Ssh(ref mut s) => s.download_changelist(f, &mut v, from, paths).await?,
            RemoteRepo::Http(ref h) => h.download_changelist(f, &mut v, from, paths).await?,
            RemoteRepo::LocalChannel(_) => HashSet::new(),
            RemoteRepo::None => unreachable!(),
        };
        Ok((r, v))
    }

    async fn dichotomy_changelist<T: MutTxnT + TxnTExt>(
        &mut self,
        txn: &ArcTxn<T>,
        remote: &pijul_core::pristine::RemoteRef<T>,
    ) -> Result<u64, error::Error<T>>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        let mut a = 0;
        let (mut b, state): (_, Merkle) = if let Some((u, v)) = txn
            .read()
            .last_remote(&remote.lock().remote)
            .map_err(error::Error::TxnErrGraph)?
        {
            debug!("dichotomy_changelist: {:?} {:?}", u, v);
            (u, (&v.b).into())
        } else {
            debug!("the local copy of the remote has no changes");
            return Ok(0);
        };
        let last_statet = if let Some((_, _, v)) = txn
            .read()
            .last_remote_tag(&remote.lock().tags)
            .map_err(error::Error::TxnErrGraph)?
        {
            v.into()
        } else {
            Merkle::zero()
        };
        debug!("last_state: {:?} {:?}", state, last_statet);
        if let Some((_, s, st)) = self.get_state(txn, Some(b)).await? {
            debug!("remote last_state: {:?} {:?}", s, st);
            if s == state && st == last_statet {
                return Ok(b + 1);
            }
        }
        while a < b {
            let mid = (a + b) / 2;
            let (mid, state) = {
                let txn = txn.read();
                let (a, b) = txn
                    .get_remote_state(&remote.lock().remote, mid)
                    .map_err(error::Error::TxnErrGraph)?
                    .unwrap();
                (a, b.b)
            };
            let statet = if let Some((_, b)) = txn
                .read()
                .get_remote_tag(&remote.lock().tags, mid)
                .map_err(error::Error::TxnErrGraph)?
            {
                b.b.into()
            } else {
                last_statet
            };

            let remote_state = self.get_state(txn, Some(mid)).await?;
            debug!("dichotomy {:?} {:?} {:?}", mid, state, remote_state);
            if let Some((_, remote_state, remote_statet)) = remote_state {
                if remote_state == state && remote_statet == statet {
                    if a == mid {
                        return Ok(a + 1);
                    } else {
                        a = mid;
                        continue;
                    }
                }
            }
            if b == mid {
                break;
            } else {
                b = mid
            }
        }
        Ok(a)
    }

    async fn get_state<T: pijul_core::TxnTExt>(
        &mut self,
        txn: &ArcTxn<T>,
        mid: Option<u64>,
    ) -> Result<Option<(u64, Merkle, Merkle)>, error::Error<T>>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        match *self {
            RemoteRepo::Local(ref mut l) => Ok(l.get_state(mid)?),
            RemoteRepo::Ssh(ref mut s) => Ok(s.get_state(mid).await?),
            RemoteRepo::Http(ref mut h) => Ok(h.get_state(mid).await?),
            RemoteRepo::LocalChannel(ref channel) => {
                if let Some(channel) = txn
                    .read()
                    .load_channel(channel.parse()?)
                    .map_err(error::Error::TxnErrGraph)?
                {
                    Ok(local::get_state(&*txn.read(), &channel, mid)?)
                } else {
                    Ok(None)
                }
            }
            RemoteRepo::None => unreachable!(),
        }
    }

    /// Send a push signature — a non-author's SSHSIG over
    /// `(state, hash, timestamp)` — authorizing this patch onto `channel`. Only
    /// a Nest records it; other peers ignore the verb. Best-effort, so failures
    /// are the caller's to warn about rather than abort the push.
    pub async fn push_signature(
        &mut self,
        channel: &str,
        hash: Hash,
        state: Merkle,
        timestamp: i64,
        sig: &[u8],
    ) -> Result<(), RemoteError> {
        match *self {
            RemoteRepo::Ssh(ref mut s) => {
                s.push_signature(channel, hash, state, timestamp, sig).await
            }
            // Push over HTTP is not an apply path on the Nest, and plain peers
            // have nowhere to put this, so it is a no-op elsewhere.
            RemoteRepo::Http(_)
            | RemoteRepo::Local(_)
            | RemoteRepo::LocalChannel(_)
            | RemoteRepo::None => Ok(()),
        }
    }

    /// Mint an HTTP bearer token by proving possession of an SSH login key.
    /// Only meaningful for HTTP remotes.
    pub async fn http_login(
        &mut self,
        skey: &pijul_core::key::SKey,
    ) -> Result<(String, u64), RemoteError> {
        match *self {
            RemoteRepo::Http(ref mut h) => h.login(skey).await,
            _ => Err(RemoteError::UnsupportedScheme(
                "http-token requires an HTTP remote".into(),
            )),
        }
    }

    async fn get_id<T: pijul_core::TxnTExt + 'static>(
        &mut self,
        txn: &ArcTxn<T>,
    ) -> Result<Option<pijul_core::pristine::RemoteId>, error::Error<T>>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        match *self {
            RemoteRepo::Local(ref l) => Ok(Some(l.get_id()?)),
            RemoteRepo::Ssh(ref mut s) => Ok(s.get_id().await?),
            RemoteRepo::Http(ref h) => Ok(h.get_id().await?),
            RemoteRepo::LocalChannel(ref channel) => {
                let txn = txn.read();
                if let Some(channel) = txn
                    .load_channel(channel.parse()?)
                    .map_err(error::Error::TxnErrGraph)?
                {
                    Ok(txn.id(&*channel.read()).cloned())
                } else {
                    Err(RemoteError::ChannelNotFound.into())
                }
            }
            RemoteRepo::None => unreachable!(),
        }
    }

    pub async fn archive<W: std::io::Write + Send + 'static>(
        &mut self,
        prefix: Option<String>,
        state: Option<(Merkle, &[Hash])>,
        umask: u16,
        w: W,
    ) -> Result<
        u64,
        error::Error<
            pijul_core::pristine::sanakirja::GenericTxn<sanakirja::MutTxn<Arc<sanakirja::Env>>>,
        >,
    > {
        match *self {
            RemoteRepo::Local(ref mut l) => {
                debug!("archiving local repo");
                let changes = pijul_core::changestore::filesystem::FileSystem::from_root(
                    &l.root,
                    pijul_repository::max_files(),
                );
                let mut tarball = pijul_core::output::Tarball::new(w, prefix, umask);
                let conflicts = if let Some((state, extra)) = state {
                    let txn = l.pristine.arc_txn_begin()?;
                    let channel = {
                        let txn = txn.read();
                        txn.load_channel(l.channel.parse()?)?.unwrap()
                    };
                    txn.archive_with_state(&changes, &channel, &state, extra, &mut tarball, 0)?
                } else {
                    let txn = l.pristine.arc_txn_begin()?;
                    let channel = {
                        let txn = txn.read();
                        txn.load_channel(l.channel.parse()?)?.unwrap()
                    };
                    txn.archive(&changes, &channel, &mut tarball)?
                };
                Ok(conflicts.len() as u64)
            }
            RemoteRepo::Ssh(ref mut s) => Ok(s.archive(prefix, state, w).await?),
            RemoteRepo::Http(ref mut h) => Ok(h.archive(prefix, state, w).await?),
            RemoteRepo::LocalChannel(_) => unreachable!(),
            RemoteRepo::None => unreachable!(),
        }
    }

    async fn download_changelist<T: MutTxnTExt>(
        &mut self,
        txn: &ArcTxn<T>,
        remote: &mut RemoteRef<T>,
        from: u64,
        paths: &[String],
    ) -> Result<HashSet<Position<Hash>>, RemoteError>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        let f = |a: &mut (&ArcTxn<T>, &mut RemoteRef<T>), n, h, m, is_tag| {
            let (ref mut txn, ref mut remote) = *a;
            let mut txn = txn.write();
            txn.put_remote(remote, n, (h, m))?;
            if is_tag {
                txn.put_tags(&mut remote.lock().tags, n, &m.into())?;
            }
            Ok(())
        };
        match *self {
            RemoteRepo::Local(ref mut l) => {
                l.download_changelist(f, &mut (txn, remote), from, paths)
            }
            RemoteRepo::Ssh(ref mut s) => {
                s.download_changelist(f, &mut (txn, remote), from, paths)
                    .await
            }
            RemoteRepo::Http(ref h) => {
                h.download_changelist(f, &mut (txn, remote), from, paths)
                    .await
            }
            RemoteRepo::LocalChannel(_) => Ok(HashSet::new()),
            RemoteRepo::None => unreachable!(),
        }
    }

    pub async fn upload_changes<T: MutTxnTExt + 'static>(
        &mut self,
        txn: &mut T,
        local: PathBuf,
        to_channel: Option<&str>,
        changes: &[CS],
    ) -> Result<(), error::Error<T>> {
        let upload_bar = ProgressBar::new(changes.len() as u64, UPLOAD_MESSAGE)?;

        match self {
            RemoteRepo::Local(l) => l.upload_changes(upload_bar, local, to_channel, changes)?,
            RemoteRepo::Ssh(s) => {
                s.upload_changes(upload_bar, local, to_channel, changes)
                    .await?
            }
            &mut RemoteRepo::Http(ref h) => {
                h.upload_changes(upload_bar, local, to_channel, changes)
                    .await?
            }
            &mut RemoteRepo::LocalChannel(ref channel) => {
                let channel_sm: pijul_core::small_string::SmallString = channel.parse()?;
                let mut channel = txn
                    .open_or_create_channel(&channel_sm)
                    .map_err(error::Error::Graph)?;
                let store = pijul_core::changestore::filesystem::FileSystem::from_changes(
                    local,
                    pijul_repository::max_files(),
                );
                local::upload_changes(upload_bar, &store, txn, &mut channel, changes)?
            }
            RemoteRepo::None => unreachable!(),
        }
        Ok(())
    }

    pub async fn download_changes(
        &mut self,
        progress_bar: ProgressBar,
        hashes: &mut tokio::sync::mpsc::UnboundedReceiver<CS>,
        send: &mut tokio::sync::mpsc::Sender<(CS, bool)>,
        path: &mut PathBuf,
        full: bool,
    ) -> Result<bool, RemoteError> {
        debug!("download_changes");
        match *self {
            RemoteRepo::Local(ref mut l) => {
                l.download_changes(progress_bar, hashes, send, path).await?
            }
            RemoteRepo::Ssh(ref mut s) => {
                s.download_changes(progress_bar, hashes, send, path, full)
                    .await?
            }
            RemoteRepo::Http(ref mut h) => {
                h.download_changes(progress_bar, hashes, send, path, full)
                    .await?
            }
            RemoteRepo::LocalChannel(_) => {
                while let Some(c) = hashes.recv().await {
                    send.send((c, true))
                        .await
                        .map_err(|_| RemoteError::ChannelClosed)?;
                }
            }
            RemoteRepo::None => unreachable!(),
        }
        Ok(true)
    }

    pub async fn update_identities<T: MutTxnTExt + TxnTExt + GraphIter>(
        &mut self,
        repo: &mut Repository,
        remote: &RemoteRef<T>,
    ) -> Result<(), RemoteError> {
        debug!("Downloading identities");
        let mut id_path = repo.path.clone();
        id_path.push(DOT_DIR);
        id_path.push("identities");
        let rev = None;
        let r = match *self {
            RemoteRepo::Local(ref mut l) => l.update_identities(rev, id_path).await?,
            RemoteRepo::Ssh(ref mut s) => s.update_identities(rev, id_path).await?,
            RemoteRepo::Http(ref mut h) => h.update_identities(rev, id_path).await?,
            RemoteRepo::LocalChannel(_) => 0,
            RemoteRepo::None => unreachable!(),
        };
        remote.set_id_revision(r);
        Ok(())
    }

    pub async fn prove(&mut self, key: pijul_core::key::SKey) -> Result<(), RemoteError> {
        match *self {
            RemoteRepo::Ssh(ref mut s) => s.prove(key).await,
            RemoteRepo::Http(ref mut h) => h.prove(key).await,
            RemoteRepo::None => unreachable!(),
            _ => Ok(()),
        }
    }

    pub async fn pull<T: MutTxnTExt + TxnTExt + GraphIter + 'static>(
        &mut self,
        repo: &mut Repository,
        txn: &ArcTxn<T>,
        channel: &mut ChannelRef<T>,
        to_apply: &[CS],
        inodes: &HashSet<Position<Hash>>,
        do_apply: bool,
    ) -> Result<Vec<CS>, error::Error<T>> {
        let apply_len = to_apply.len() as u64;
        let download_bar = ProgressBar::new(apply_len, DOWNLOAD_MESSAGE)?;
        let apply_bar = if do_apply {
            Some(ProgressBar::new(apply_len, APPLY_MESSAGE)?)
        } else {
            None
        };

        let (mut send, recv) = tokio::sync::mpsc::channel(100);

        let mut self_ = std::mem::replace(self, RemoteRepo::None);
        let (hash_send, mut hash_recv) = tokio::sync::mpsc::unbounded_channel();
        let mut change_path_ = repo.path.clone();
        change_path_.push(DOT_DIR);
        change_path_.push("changes");
        let cloned_download_bar = download_bar.clone();
        let t = tokio::spawn(async move {
            self_
                .download_changes(
                    cloned_download_bar,
                    &mut hash_recv,
                    &mut send,
                    &mut change_path_,
                    false,
                )
                .await?;

            Ok::<_, RemoteError>(self_)
        });

        let mut change_path_ = repo.changes_dir.clone();
        let mut waiting = 0;
        let (send_ready, mut recv_ready) = tokio::sync::mpsc::channel(100);

        let mut asked = HashSet::new();
        for h in to_apply {
            debug!("to_apply {:?}", h);
            match h {
                CS::Change(h) => {
                    pijul_core::changestore::filesystem::push_filename(&mut change_path_, h);
                }
                CS::State(h) => {
                    pijul_core::changestore::filesystem::push_tag_filename(&mut change_path_, h);
                }
            }
            asked.insert(*h);
            hash_send.send(*h).map_err(|_| RemoteError::ChannelClosed)?;
            waiting += 1;
            pijul_core::changestore::filesystem::pop_filename(&mut change_path_);
        }

        let u = self
            .download_changes_rec(
                repo,
                hash_send,
                recv,
                send_ready,
                download_bar,
                waiting,
                asked,
            )
            .await?;

        let mut ws = pijul_core::ApplyWorkspace::new();
        let mut to_apply_inodes = HashSet::new();
        while let Some(h) = recv_ready.recv().await {
            debug!("to_apply: {:?}", h);
            let touches_inodes = inodes.is_empty()
                || {
                    debug!("inodes = {:?}", inodes);
                    use pijul_core::changestore::ChangeStore;
                    if let CS::Change(ref h) = h {
                        let changes = repo.changes.get_changes(h).map_err(RemoteError::from)?;
                        changes.iter().any(|c| {
                            c.iter().any(|c| {
                                let inode = c.inode();
                                debug!("inode = {:?}", inode);
                                inodes.contains(&Position {
                                    change: inode.change.unwrap_or(*h),
                                    pos: inode.pos,
                                })
                            })
                        })
                    } else {
                        false
                    }
                }
                || { inodes.iter().any(|i| CS::Change(i.change) == h) };

            if touches_inodes {
                to_apply_inodes.insert(h);
            } else {
                continue;
            }

            if let Some(apply_bar) = apply_bar.clone() {
                info!("Applying {:?}", h);
                apply_bar.inc(1);
                debug!("apply");
                if let CS::Change(h) = h {
                    let mut channel = channel.write();
                    txn.write()
                        .apply_change_rec_ws(&repo.changes, &mut channel, &h, &mut ws)?;
                }
                debug!("applied");
            } else {
                debug!("not applying {:?}", h)
            }
        }

        let mut result = Vec::with_capacity(to_apply_inodes.len());
        for h in to_apply {
            if to_apply_inodes.contains(&h) {
                result.push(*h)
            }
        }

        debug!("finished");
        debug!("waiting for spawned process");
        *self = t.await.map_err(|_| error::Error::Concurrency)??;
        u.await.map_err(|_| error::Error::Concurrency)??;
        Ok(result)
    }

    async fn download_changes_rec(
        &mut self,
        repo: &mut Repository,
        send_hash: tokio::sync::mpsc::UnboundedSender<CS>,
        mut recv_signal: tokio::sync::mpsc::Receiver<(CS, bool)>,
        send_ready: tokio::sync::mpsc::Sender<CS>,
        progress_bar: ProgressBar,
        mut waiting: usize,
        mut asked: HashSet<CS>,
    ) -> Result<tokio::task::JoinHandle<Result<(), RemoteError>>, RemoteError> {
        let mut dep_path = repo.changes_dir.clone();
        let changes = repo.changes.clone();
        let t = tokio::spawn(async move {
            if waiting == 0 {
                return Ok(());
            }
            let mut ready = Vec::new();
            while let Some((hash, follow)) = recv_signal.recv().await {
                debug!("received {:?} {:?}", hash, follow);
                if let CS::Change(hash) = hash {
                    waiting -= 1;
                    if follow {
                        use pijul_core::changestore::ChangeStore;
                        let mut needs_dep = false;
                        for dep in changes.get_dependencies(&hash).map_err(RemoteError::from)? {
                            let dep: pijul_core::pristine::Hash = dep;

                            pijul_core::changestore::filesystem::push_filename(&mut dep_path, &dep);
                            let has_dep = std::fs::metadata(&dep_path).is_ok();
                            pijul_core::changestore::filesystem::pop_filename(&mut dep_path);

                            if !has_dep {
                                needs_dep = true;
                                if asked.insert(CS::Change(dep)) {
                                    progress_bar.inc(1);
                                    send_hash
                                        .send(CS::Change(dep))
                                        .map_err(|_| RemoteError::ChannelClosed)?;
                                    waiting += 1
                                }
                            }
                        }

                        if !needs_dep {
                            send_ready
                                .send(CS::Change(hash))
                                .await
                                .map_err(|_| RemoteError::ChannelClosed)?;
                        } else {
                            ready.push(CS::Change(hash))
                        }
                    } else {
                        send_ready
                            .send(CS::Change(hash))
                            .await
                            .map_err(|_| RemoteError::ChannelClosed)?;
                    }
                }
                if waiting == 0 {
                    break;
                }
            }
            info!("waiting loop done");
            for r in ready {
                send_ready
                    .send(r)
                    .await
                    .map_err(|_| RemoteError::ChannelClosed)?;
            }
            std::mem::drop(recv_signal);
            Ok(())
        });
        Ok(t)
    }

    pub async fn clone_tag<T: MutTxnTExt + TxnTExt + GraphIter + 'static>(
        &mut self,
        repo: &mut Repository,
        txn: &ArcTxn<T>,
        channel: &mut ChannelRef<T>,
        tag: &[Hash],
    ) -> Result<(), error::Error<T>>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        let (send_hash, mut recv_hash) = tokio::sync::mpsc::unbounded_channel();
        let (mut send_signal, recv_signal) = tokio::sync::mpsc::channel(100);
        let mut self_ = std::mem::replace(self, RemoteRepo::None);
        let mut change_path_ = repo.changes_dir.clone();
        let download_bar = ProgressBar::new(tag.len() as u64, DOWNLOAD_MESSAGE)?;
        let cloned_download_bar = download_bar.clone();

        let t = tokio::spawn(async move {
            self_
                .download_changes(
                    cloned_download_bar,
                    &mut recv_hash,
                    &mut send_signal,
                    &mut change_path_,
                    false,
                )
                .await?;
            Ok::<_, RemoteError>(self_)
        });

        let mut waiting = 0;
        let mut asked = HashSet::new();
        for &h in tag.iter() {
            waiting += 1;
            send_hash
                .send(CS::Change(h))
                .map_err(|_| RemoteError::ChannelClosed)?;
            asked.insert(CS::Change(h));
        }

        let (send_ready, mut recv_ready) = tokio::sync::mpsc::channel(100);

        let u = self
            .download_changes_rec(
                repo,
                send_hash,
                recv_signal,
                send_ready,
                download_bar,
                waiting,
                asked,
            )
            .await?;

        let mut hashes = Vec::new();
        let mut ws = pijul_core::ApplyWorkspace::new();
        {
            let mut channel_ = channel.write();
            let mut txn = txn.write();
            while let Some(hash) = recv_ready.recv().await {
                if let CS::Change(ref hash) = hash {
                    txn.apply_change_rec_ws(&repo.changes, &mut channel_, hash, &mut ws)?;
                }
                hashes.push(hash);
            }
        }
        let r: Result<_, RemoteError> = t.await.map_err(|_| error::Error::Concurrency)?;
        *self = r?;
        u.await.map_err(|_| error::Error::Concurrency)??;
        self.complete_changes(repo, txn, channel, &hashes, false)
            .await?;
        Ok(())
    }

    pub async fn clone_state<T: MutTxnTExt + TxnTExt + GraphIter + 'static>(
        &mut self,
        repo: &mut Repository,
        txn: &ArcTxn<T>,
        channel: &mut ChannelRef<T>,
        state: Merkle,
    ) -> Result<(), error::Error<T>>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        let id = if let Some(id) = self.get_id(txn).await? {
            id
        } else {
            return Ok(());
        };
        debug!("update_changelist");
        self.update_changelist(txn, &[]).await?;
        debug!("create");
        let name_sm: pijul_core::small_string::SmallString = self.name().unwrap().parse()?;
        let remote = txn
            .write()
            .open_or_create_remote(id, &name_sm)
            .map_err(error::Error::Graph)?;
        let mut to_pull = Vec::new();
        let mut found = false;
        for x in txn
            .read()
            .iter_remote(&remote.lock().remote, 0)
            .map_err(error::Error::TxnErrGraph)?
        {
            let (n, p) = x.map_err(error::Error::TxnErrGraph)?;
            debug!("{:?} {:?}", n, p);
            to_pull.push(CS::Change(p.a.into()));
            if p.b == state {
                found = true;
                break;
            }
        }
        if !found {
            return Err(RemoteError::StateNotFound(state).into());
        }
        self.pull(repo, txn, channel, &to_pull, &HashSet::new(), true)
            .await?;
        self.update_identities(repo, &remote).await?;

        self.complete_changes(repo, txn, channel, &to_pull, false)
            .await?;
        Ok(())
    }

    pub async fn complete_changes<T: MutTxnT + TxnTExt + GraphIter>(
        &mut self,
        repo: &pijul_repository::Repository,
        txn: &ArcTxn<T>,
        local_channel: &mut ChannelRef<T>,
        changes: &[CS],
        full: bool,
    ) -> Result<(), RemoteError>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        debug!("complete changes {:?}", changes);
        use pijul_core::changestore::ChangeStore;
        let (send_hash, mut recv_hash) = tokio::sync::mpsc::unbounded_channel();
        let (mut send_sig, mut recv_sig) = tokio::sync::mpsc::channel(100);
        let mut self_ = std::mem::replace(self, RemoteRepo::None);
        let mut changes_dir = repo.changes_dir.clone();

        let download_bar = ProgressBar::new(changes.len() as u64, DOWNLOAD_MESSAGE)?;
        let _completion_spinner = Spinner::new(COMPLETE_MESSAGE)?;
        let t: tokio::task::JoinHandle<Result<RemoteRepo, RemoteError>> =
            tokio::spawn(async move {
                self_
                    .download_changes(
                        download_bar,
                        &mut recv_hash,
                        &mut send_sig,
                        &mut changes_dir,
                        true,
                    )
                    .await?;
                Ok::<_, RemoteError>(self_)
            });

        {
            let txn = txn.read();
            for c in changes {
                let c = if let CS::Change(c) = c { c } else { continue };
                let sc = c.into();
                if repo
                    .changes
                    .has_contents(*c, txn.get_internal(&sc)?.cloned())
                {
                    debug!("has contents {:?}", c);
                    continue;
                }
                if full {
                    debug!("sending send_hash");
                    send_hash
                        .send(CS::Change(*c))
                        .map_err(|_| RemoteError::ChannelClosed)?;
                    debug!("sent");
                    continue;
                }
                let change = if let Some(&i) = txn.get_internal(&sc)? {
                    i
                } else {
                    debug!("could not find internal for {:?}", sc);
                    continue;
                };
                let v = pijul_core::pristine::Vertex {
                    change,
                    start: pijul_core::pristine::ChangePosition(0u64.into()),
                    end: pijul_core::pristine::ChangePosition(0u64.into()),
                };
                let channel = local_channel.read();
                let graph = txn.graph(&channel);
                for x in txn.iter_graph(graph, Some(&v))? {
                    let (v, e) = x?;
                    if v.change > change {
                        break;
                    } else if e.flag().is_alive_parent() {
                        send_hash
                            .send(CS::Change(*c))
                            .map_err(|_| RemoteError::ChannelClosed)?;
                        break;
                    }
                }
            }
        }
        debug!("dropping send_hash");
        std::mem::drop(send_hash);
        while recv_sig.recv().await.is_some() {}
        *self = t.await??;
        Ok(())
    }

    pub async fn clone_channel<T: MutTxnTExt + TxnTExt + GraphIter + 'static>(
        &mut self,
        repo: &mut Repository,
        txn: &ArcTxn<T>,
        local_channel: &mut ChannelRef<T>,
        path: &[String],
    ) -> Result<(), error::Error<T>>
    where
        RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
    {
        let (inodes, remote_changes) = if let Some(x) = self.update_changelist(txn, path).await? {
            x
        } else {
            return Err(RemoteError::ChannelNotFound.into());
        };
        let mut pullable = Vec::new();
        {
            let rem = remote_changes.lock();
            for x in txn
                .read()
                .iter_remote(&rem.remote, 0)
                .map_err(error::Error::TxnErrGraph)?
            {
                let (_, p) = x.map_err(error::Error::TxnErrGraph)?;
                pullable.push(CS::Change(p.a.into()))
            }
        }
        self.pull(repo, txn, local_channel, &pullable, &inodes, true)
            .await?;
        self.update_identities(repo, &remote_changes).await?;

        self.complete_changes(repo, txn, local_channel, &pullable, false)
            .await?;
        Ok(())
    }
}

static CHANGELIST_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?P<num>[0-9]+)\.(?P<hash>[A-Za-z0-9]+)\.(?P<merkle>[A-Za-z0-9]+)(?P<tag>\.)?"#)
        .unwrap()
});
static PATHS_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?P<hash>[A-Za-z0-9]+)\.(?P<num>[0-9]+)"#).unwrap());

enum ListLine {
    Change {
        n: u64,
        h: Hash,
        m: Merkle,
        tag: bool,
    },
    Position(Position<Hash>),
    Error(String),
}

fn parse_line(data: &str) -> Result<ListLine, RemoteError> {
    debug!("data = {:?}", data);
    if let Some(caps) = CHANGELIST_LINE.captures(data) {
        if let (Some(h), Some(m)) = (
            Hash::from_base32(caps.name("hash").unwrap().as_str().as_bytes()),
            Merkle::from_base32(caps.name("merkle").unwrap().as_str().as_bytes()),
        ) {
            return Ok(ListLine::Change {
                n: caps.name("num").unwrap().as_str().parse().unwrap(),
                h,
                m,
                tag: caps.name("tag").is_some(),
            });
        }
    }
    if data.starts_with("error:") {
        return Ok(ListLine::Error(data.split_at(6).1.to_string()));
    }
    if let Some(caps) = PATHS_LINE.captures(data) {
        return Ok(ListLine::Position(Position {
            change: Hash::from_base32(caps.name("hash").unwrap().as_str().as_bytes()).unwrap(),
            pos: ChangePosition(
                caps.name("num")
                    .unwrap()
                    .as_str()
                    .parse::<u64>()
                    .unwrap()
                    .into(),
            ),
        }));
    }
    debug!("offending line: {:?}", data);
    Err(RemoteError::ProtocolError)
}

fn remote_unrecs<T: TxnTExt + ChannelTxnT>(
    txn: &T,
    current_channel: &ChannelRef<T>,
    ours_ge_dichotomy: &[(u64, CS)],
    theirs_ge_dichotomy_set: &HashSet<CS>,
) -> Result<Vec<(u64, CS)>, error::Error<T>>
where
    RemoteError: From<T::GraphError> + From<pijul_core::pristine::TxnErr<T::GraphError>>,
{
    let mut remote_unrecs = Vec::new();
    for (n, hash) in ours_ge_dichotomy {
        debug!("ours_ge_dichotomy: {:?} {:?}", n, hash);
        if theirs_ge_dichotomy_set.contains(hash) {
            debug!("still present");
            continue;
        } else {
            let has_it = match hash {
                CS::Change(hash) => txn
                    .get_revchanges(&current_channel, &hash)
                    .map_err(crate::error::Error::Graph)?
                    .is_some(),
                CS::State(state) => {
                    let ch = current_channel.read();
                    if let Some(n) = txn
                        .channel_has_state(txn.states(&*ch), &state.into())
                        .map_err(crate::error::Error::TxnErrGraph)?
                    {
                        txn.is_tagged(txn.tags(&*ch), n.into())
                            .map_err(crate::error::Error::TxnErrGraph)?
                    } else {
                        false
                    }
                }
            };
            if has_it {
                remote_unrecs.push((*n, *hash))
            } else {
                continue;
            }
        }
    }
    Ok(remote_unrecs)
}
