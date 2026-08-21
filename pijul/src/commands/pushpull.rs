use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::io::{Stdout, Write};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::bail;
use clap::{Parser, ValueHint};
use log::debug;
use regex::Regex;

use super::{get_channel, make_changelist, parse_changelist};
use crate::commands::common_opts::RepoPath;
use pijul_config::remote::{RemoteConfig, RemoteHttpHeader};
use pijul_core::changestore::ChangeStore;
use pijul_core::pristine::RemoteId;
use pijul_core::pristine::sanakirja::{MutTxn, RawMutTxnT};
use pijul_core::*;
use pijul_interaction::{APPLY_MESSAGE, OUTPUT_MESSAGE, ProgressBar, Spinner};
use pijul_remote::{self as remote, CS, PushDelta, RemoteDelta, RemoteRepo};
use pijul_repository::Repository;

#[derive(Parser, Debug)]
pub struct Remote {
    #[clap(flatten)]
    base: RepoPath,
    #[clap(subcommand)]
    subcmd: Option<SubRemote>,
}

#[derive(Parser, Debug)]
pub enum SubRemote {
    /// Set the default remote
    #[clap(name = "default")]
    Default { remote: String },
    /// Deletes the remote
    #[clap(name = "delete")]
    Delete {
        remote: String,
        /// Delete only the specified datum, i.e. database ID or remote name. If this isn't specified and there is at most one of each kind of data linked to a remote, the entire remote is deleted.
        #[arg(group = "behavior", short = '1', long = "exact")]
        exact: bool,
        /// Delete all the linked data of the remote, i.e. all names and all database IDs.
        #[arg(group = "behavior", long = "flood")]
        flood: bool,
    },
}

#[derive(Default)]
struct RemoteInfo {
    id: BTreeSet<RemoteId>,
    path: String,
    configs: BTreeMap<String, ExtraConfig>,
    default: bool,
}

struct RemoteInfos {
    data: Vec<RemoteInfo>,
    by_id: BTreeMap<RemoteId, usize>,
    by_url: BTreeMap<String, usize>,
    by_label: BTreeMap<String, usize>,
}

struct ExtraConfig {
    headers: BTreeMap<String, String>,
    default: bool,
}

/// Aggregates remote information from three sources:
/// - the repo database
/// - named remotes in the configuration
/// - the default remote set in the configuration
fn aggregate_remote_info<T>(
    config: &pijul_config::Config,
    txn: &T,
) -> Result<RemoteInfos, anyhow::Error>
where
    T: TxnT,
{
    let mut data = Vec::new();
    let mut by_id = BTreeMap::new();
    let mut by_url = BTreeMap::new();
    let mut by_label = BTreeMap::new();
    let mut by_db_name = BTreeMap::new();

    for rc in &config.remotes {
        let idx = by_url.get(rc.url()).copied().unwrap_or_else(|| {
            data.push(RemoteInfo {
                path: rc.url().to_string(),
                ..Default::default()
            });
            data.len() - 1
        });

        if rc.db_uses_name() {
            by_db_name.insert(rc.name().to_string(), idx);
        }

        by_db_name.insert(rc.url().to_string(), idx);
        by_label.insert(rc.name().to_string(), idx);
        by_url.insert(rc.url().to_string(), idx);

        let d = &mut data[idx];

        let headers = match rc {
            RemoteConfig::Ssh { .. } => BTreeMap::new(),
            RemoteConfig::Http { headers, .. } => headers
                .iter()
                .map(|(k, v)| {
                    let v = match v {
                        RemoteHttpHeader::String(s) => s.clone(),
                        RemoteHttpHeader::Shell(s) => s.shell.clone(),
                    };
                    (k.clone(), v)
                })
                .collect(),
        };

        d.configs.insert(
            rc.name().to_string(),
            ExtraConfig {
                headers,
                // set later
                default: false,
            },
        );
    }

    for r in txn.iter_remotes(&RemoteId::nil())? {
        let r = r?;
        let lock = r.lock();

        let idx = by_db_name
            .get(lock.path.as_str())
            .copied()
            .unwrap_or_else(|| {
                data.push(RemoteInfo {
                    path: lock.path.as_str().to_string(),
                    ..Default::default()
                });

                by_url.insert(lock.path.as_str().to_string(), data.len() - 1);
                data.len() - 1
            });

        data[idx].id.insert(*r.id());
        by_id.insert(*r.id(), idx);
    }

    if let Some(default) = &config.default_remote {
        let label_idx = by_label.get(default);

        let idx = label_idx
            .or_else(|| by_url.get(default))
            .copied()
            .unwrap_or_else(|| {
                data.push(RemoteInfo {
                    path: default.to_string(),
                    ..Default::default()
                });

                by_url.insert(default.to_string(), data.len() - 1);
                data.len() - 1
            });

        data[idx].default = true;

        if label_idx.is_some() {
            for (label, ec) in data[idx].configs.iter_mut() {
                if label == default {
                    ec.default = true;
                }
            }
        }
    }

    Ok(RemoteInfos {
        data,
        by_id,
        by_url,
        by_label,
    })
}

impl Remote {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;
        debug!("{:?}", config);
        let mut stdout = io::stdout();
        match self.subcmd {
            None => {
                let txn = repo.pristine.txn_begin()?;
                let remote_infos = aggregate_remote_info(config, &txn)?;

                for info in remote_infos.data {
                    let can_collapse = info.configs.len() < 2
                        || info.configs.iter().all(|(_, el)| el.headers.is_empty());
                    let mut flag = ' ';

                    if info.default
                        && (can_collapse || info.configs.iter().all(|(_, c)| !c.default))
                    {
                        flag = '*';
                    }

                    // Under normal circumstances, there should only be at most
                    // one ID here. However, still do our best to format it in a
                    // reasonably nice way.
                    let mut ids = info.id.iter();

                    write!(stdout, "{} ", flag)?;

                    if let Some(id) = ids.next() {
                        write!(stdout, "{}", id)?;
                    } else {
                        write!(stdout, "{:26}", "(no ID)")?;
                    }

                    write!(stdout, ": ")?;

                    fn write_headers(stdout: &mut Stdout, c: &ExtraConfig) -> io::Result<()> {
                        if !c.headers.is_empty() {
                            writeln!(stdout, "    Headers:")?;

                            for (header, value) in &c.headers {
                                writeln!(stdout, "    - {}: {}", header, value)?;
                            }
                        }

                        Ok(())
                    }

                    if can_collapse {
                        for (name, _) in &info.configs {
                            write!(stdout, "«{}» ", name)?;
                        }

                        writeln!(stdout, "{}", info.path)?;

                        for (_, c) in &info.configs {
                            write_headers(&mut stdout, c)?;
                        }
                    } else {
                        writeln!(stdout, "{}", info.path)?;

                        for (name, c) in &info.configs {
                            let mut flag = ' ';

                            if c.default {
                                flag = '*';
                            }

                            writeln!(stdout, "{} «{}»", flag, name)?;
                            write_headers(&mut stdout, c)?;
                        }
                    }

                    if let Some(extra_id) = ids.next() {
                        write!(stdout, "  (also registered as {}", extra_id)?;

                        while let Some(extra_id) = ids.next() {
                            write!(stdout, ", {}", extra_id)?;
                        }

                        writeln!(stdout, ")")?;
                    }
                }
            }
            Some(SubRemote::Default { remote }) => {
                if let Some(mut local_config) = config.local() {
                    local_config.default_remote = Some(remote);
                    local_config.write()?;
                }
            }
            Some(SubRemote::Delete {
                remote: spec,
                exact,
                flood,
            }) => {
                let mut txn = repo.pristine.mut_txn_begin()?;

                let db = aggregate_remote_info(config, &mut txn)?;

                let label_idx = db.by_label.get(&spec);
                let remote = RemoteId::from_base32(spec.as_bytes());
                let remote_idx = remote.and_then(|r| db.by_id.get(&r));
                let url_idx = db.by_url.get(&spec);

                let Some(&idx) = label_idx.or(remote_idx).or(url_idx) else {
                    bail!("No such remote: {}", spec);
                };

                let d = &db.data[idx];

                fn remove_named_remote(local_config: &mut pijul_config::local::Local, name: &str) {
                    let idx = local_config
                        .remotes
                        .iter()
                        .position(|c| c.name() == name)
                        .unwrap();
                    local_config.remotes.remove(idx);
                }

                fn flood_delete<T: RawMutTxnT>(
                    exact: bool,
                    flood: bool,
                    d: &RemoteInfo,
                    local_config: &mut pijul_config::local::Local,
                    txn: &mut MutTxn<T>,
                ) -> Result<(), anyhow::Error> {
                    let mut delete_count = 0;

                    for id in &d.id {
                        txn.drop_named_remote(*id)?;
                        delete_count += 1;
                    }

                    if !flood {
                        // just in case, but these should never trigger because
                        // of the checks beforehand
                        assert!(delete_count <= 1);
                    }

                    if !exact {
                        delete_count = 0;
                    }

                    for (name, _ec) in &d.configs {
                        remove_named_remote(local_config, name);
                        delete_count += 1;
                    }

                    if !flood {
                        assert!(delete_count <= 1);
                    }

                    if d.default {
                        local_config.default_remote = None;
                    }

                    Ok(())
                }

                let mut local_config = config.local().unwrap();
                if flood {
                    flood_delete(flood, exact, d, &mut local_config, &mut txn)?;
                } else {
                    // all the flood_delete calls in this block are ensured to only delete at most one thing

                    if label_idx.is_some() {
                        if !exact && d.id.len() <= 1 && d.configs.len() <= 1 {
                            flood_delete(flood, exact, d, &mut local_config, &mut txn)?;
                        } else {
                            let idx = local_config
                                .remotes
                                .iter()
                                .position(|c| c.name() == spec)
                                .unwrap();
                            local_config.remotes.remove(idx);

                            if d.configs.get(&spec).unwrap().default {
                                local_config.default_remote = None;
                            }
                        }
                    } else if remote_idx.is_some() {
                        if !exact && d.id.len() <= 1 && d.configs.len() <= 1 {
                            flood_delete(flood, exact, d, &mut local_config, &mut txn)?;
                        } else {
                            txn.drop_named_remote(remote.unwrap())?;
                        }
                    } else if url_idx.is_some() {
                        if exact && !d.configs.is_empty() && !d.id.is_empty() {
                            bail!(
                                "Cannot delete '{}' since there is both a named remote and an ID associated with it",
                                spec
                            );
                        }

                        if d.configs.len() > 1 {
                            bail!(
                                "Cannot delete '{}' since there are multiple named remotes associated with it",
                                spec
                            );
                        }

                        if d.id.len() > 1 {
                            bail!(
                                "Cannot delete '{}' since there are multiple IDs associated with it",
                                spec
                            );
                        }

                        flood_delete(flood, exact, d, &mut local_config, &mut txn)?;
                    }
                }

                txn.commit()?;
                local_config.write()?;
            }
        }
        Ok(())
    }
}

#[derive(Parser, Debug)]
pub struct Push {
    #[clap(flatten)]
    base: RepoPath,
    /// Push from this channel instead of the default channel
    #[clap(long = "from-channel")]
    from_channel: Option<String>,
    /// Push all changes
    #[clap(long = "all", short = 'a', conflicts_with = "changes")]
    all: bool,
    /// Do not check certificates (HTTPS remotes only, this option might be dangerous)
    #[clap(short = 'k')]
    no_cert_check: bool,
    /// Do not open an editor to report changes or unrecords that are new on the
    /// remote; push without the interactive notification (for scripts/agents)
    #[clap(long = "no-notify")]
    no_notify: bool,
    /// Push changes only relating to these paths
    #[clap(long = "path", value_hint = ValueHint::AnyPath)]
    path: Vec<String>,
    /// Push to this remote
    to: Option<String>,
    /// Push to this remote channel instead of the remote's default channel
    #[clap(long = "to-channel")]
    to_channel: Option<String>,
    /// Push only these changes
    #[clap(last = true)]
    changes: Vec<String>,
}

#[derive(Parser, Debug)]
pub struct Pull {
    #[clap(flatten)]
    base: RepoPath,
    /// Pull into this channel instead of the current channel
    #[clap(long = "to-channel")]
    to_channel: Option<String>,
    /// Pull all changes
    #[clap(long = "all", short = 'a', conflicts_with = "changes")]
    all: bool,
    /// Do not open an editor to notify about remote unrecords (for scripts/agents)
    #[clap(long = "no-notify")]
    no_notify: bool,
    /// Print the resulting conflicts as a JSON object on stdout instead of the
    /// human-readable list (for scripts/agents): {"conflicts":[{"kind","path"[,"line"]}]}
    #[clap(long = "json")]
    json: bool,
    /// Do not check certificates (HTTPS remotes only, this option might be dangerous)
    #[clap(short = 'k')]
    no_cert_check: bool,
    /// Download complete changes, including file contents (needed if you lose network access later)
    #[clap(long = "complete")]
    complete: bool,
    /// Only pull to these paths
    #[clap(long = "path", value_hint = ValueHint::AnyPath)]
    path: Vec<String>,
    /// Pull from this remote
    from: Option<String>,
    /// Pull from this remote channel
    #[clap(long = "from-channel")]
    from_channel: Option<String>,
    /// Pull changes from the local repository, not necessarily from a channel
    #[clap(last = true)]
    changes: Vec<String>, // For local changes only, can't be symmetric.
}

static CHANNEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([^:]*)(:(.*))?"#).unwrap());

impl Push {
    /// Gets the `to_upload` vector while trying to auto-update
    /// the local cache if possible. Also calculates whether the remote
    /// has any changes we don't know about.
    async fn to_upload<T: RawMutTxnT + 'static>(
        &self,
        txn: &ArcTxn<MutTxn<T>>,
        channel: &mut ChannelRef<MutTxn<T>>,
        repo: &Repository,
        remote: &mut RemoteRepo,
    ) -> Result<PushDelta, anyhow::Error> {
        let remote_delta = remote
            .update_changelist_pushpull(
                txn,
                &self.path,
                channel,
                None,
                repo,
                self.changes.as_slice(),
                false,
            )
            .await?;
        Ok(
            if let &mut RemoteRepo::LocalChannel(ref remote_channel) = remote {
                remote_delta.to_local_channel_push(
                    txn,
                    remote_channel,
                    self.path.as_slice(),
                    channel,
                    repo,
                )?
            } else {
                remote_delta.to_remote_push(
                    &mut *txn.write(),
                    self.path.as_slice(),
                    channel,
                    repo,
                )?
            },
        )
    }

    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub async fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let mut stderr = std::io::stderr();
        let repo = self.base.find_root()?;
        let txn = repo.pristine.arc_txn_begin()?;
        let channel_name = get_channel(self.from_channel.as_deref(), &*txn.read())
            .0
            .to_string();
        let remote_name = if let Some(ref rem) = self.to {
            rem
        } else if let Some(ref def) = config.default_remote {
            def
        } else {
            bail!("Missing remote");
        };

        let (remote_channel, push_channel) = self
            .to_channel
            .as_deref()
            .map(|c| {
                let c = CHANNEL.captures(c).unwrap();
                let push_channel = c.get(3).map(|x| x.as_str());
                let remote_channel = Some(c.get(1).unwrap().as_str()).filter(|v| !v.is_empty());
                (remote_channel, push_channel)
            })
            .unwrap_or_default();
        let remote_channel = remote_channel.unwrap_or(&channel_name);

        debug!("remote_channel = {:?} {:?}", remote_channel, push_channel);
        let mut remote = remote::repository(
            config,
            Some(&repo.path),
            None,
            &remote_name,
            remote_channel,
            self.no_cert_check,
            true,
        )
        .await?;

        let channel_sm: pijul_core::small_string::SmallString = channel_name.parse()?;
        let mut channel = txn.write().open_or_create_channel(&channel_sm)?;

        let PushDelta {
            to_upload,
            remote_unrecs,
            unknown_changes,
            remote_state,
            ..
        } = self
            .to_upload(&txn, &mut channel, &repo, &mut remote)
            .await?;

        debug!("to_upload = {:?}", to_upload);

        // Filter out pinned patches (and implicitly-pinned ones above the watermark).
        let to_upload = if !config.pins.is_empty() {
            let txn_r = txn.read();
            let ch_r = channel.read();
            let pins = config.pins.clone();
            let is_pinned = |h: pijul_core::Hash| pins.iter().any(|p| *p == h.to_base32());
            let watermark = pijul_core::pin::pinned_watermark(&*txn_r, &*ch_r, is_pinned)?;
            drop(ch_r);
            if let Some(wm) = watermark {
                to_upload
                    .into_iter()
                    .filter(|cs| match cs {
                        CS::Change(h) => txn_r
                            .get_revchanges(&channel, h)
                            .ok()
                            .flatten()
                            .map(|pos| u64::from(pos) < wm)
                            .unwrap_or(true),
                        CS::State(s) => {
                            let ch_r2 = channel.read();
                            txn_r
                                .channel_has_state(txn_r.states(&*ch_r2), &(*s).into())
                                .ok()
                                .flatten()
                                .map(|pos| u64::from(pos) < wm)
                                .unwrap_or(true)
                        }
                    })
                    .collect()
            } else {
                to_upload
            }
        } else {
            to_upload
        };

        if to_upload.is_empty() {
            writeln!(stderr, "Nothing to push")?;
            txn.commit()?;
            return Ok(());
        }

        if !self.no_notify {
            notify_remote_unrecords(&repo, remote_unrecs.as_slice());
            notify_unknown_changes(unknown_changes.as_slice());
        }

        let to_upload = if !self.changes.is_empty() {
            let mut u: Vec<CS> = Vec::new();
            let mut not_found = Vec::new();
            let txn = txn.read();
            for change in self.changes.iter() {
                match txn.hash_from_prefix(change) {
                    Ok((hash, _)) => {
                        if to_upload.contains(&CS::Change(hash)) {
                            u.push(CS::Change(hash));
                        }
                    }
                    Err(_) => {
                        if !not_found.contains(change) {
                            not_found.push(change.to_string());
                        }
                    }
                }
            }

            u.sort_by(|a, b| match (a, b) {
                (CS::Change(a), CS::Change(b)) => {
                    let na = txn.get_revchanges(&channel, a).unwrap().unwrap();
                    let nb = txn.get_revchanges(&channel, b).unwrap().unwrap();
                    na.cmp(&nb)
                }
                (CS::State(a), CS::State(b)) => {
                    let na = txn
                        .channel_has_state(txn.states(&*channel.read()), &a.into())
                        .unwrap()
                        .unwrap();
                    let nb = txn
                        .channel_has_state(txn.states(&*channel.read()), &b.into())
                        .unwrap()
                        .unwrap();
                    na.cmp(&nb)
                }
                _ => unreachable!(),
            });

            if !not_found.is_empty() {
                bail!("Changes not found: {:?}", not_found)
            }

            check_deps(&repo.changes, &to_upload, &u)?;
            u
        } else if self.all {
            to_upload
        } else {
            let mut o = make_changelist(&repo.changes, &to_upload, "push")?;
            loop {
                let d = parse_changelist(&edit::edit_bytes(&o[..])?, &to_upload);
                let comp = complete_deps(&repo.changes, Some(&to_upload), &d)?;
                if comp.len() == d.len() {
                    break comp;
                }
                o = make_changelist(&repo.changes, &comp, "push")?
            }
        };
        debug!("to_upload = {:?}", to_upload);

        if to_upload.is_empty() {
            writeln!(stderr, "Nothing to push")?;
            txn.commit()?;
            return Ok(());
        }

        // Push signatures: sign (remote head, patch, now) for each change and
        // send them ahead of the applies. A Nest with `min_push_signatures > 0`
        // uses them as the second-actor authorization; other peers ignore them.
        // The head comes from the just-synced remote cache (no round-trip).
        // Entirely best-effort — a missing identity or signing error must never
        // block a push, so every failure is a warning, not an abort.
        let effective_channel = push_channel.unwrap_or(remote_channel).to_string();
        if let Err(e) = send_push_signatures(
            config,
            &mut remote,
            &effective_channel,
            remote_state,
            &to_upload,
        )
        .await
        {
            writeln!(stderr, "Warning: could not send push signatures: {}", e)?;
        }

        remote
            .upload_changes(
                &mut *txn.write(),
                repo.changes_dir.clone(),
                push_channel,
                &to_upload,
            )
            .await?;

        txn.commit()?;

        remote.finish().await?;
        Ok(())
    }
}

/// Sign `(remote head, patch, now)` for each change being pushed and stream the
/// signatures to the remote ahead of the applies. `state` is the remote head the
/// changelist exchange already cached — shared by the whole batch, so every
/// signature commits to the position the pusher observed, with no extra
/// round-trip (an in-band `state` query here stalls the push). No-op unless a
/// remote head is known and an identity is available; all failures propagate to
/// the caller as a single warning.
async fn send_push_signatures(
    config: &pijul_config::Config,
    remote: &mut RemoteRepo,
    channel: &str,
    state: Option<Merkle>,
    to_upload: &[CS],
) -> Result<(), anyhow::Error> {
    // No known remote head (fresh/empty remote, or a non-Nest peer) ⇒ nothing to
    // bind a signature to.
    let Some(state) = state else {
        return Ok(());
    };

    // Best-effort identity: with none configured there is nothing to sign with,
    // and we must not trigger interactive identity setup during a push.
    if pijul_identity::Complete::load_all()
        .map(|v| v.is_empty())
        .unwrap_or(true)
    {
        return Ok(());
    }
    let name = pijul_identity::choose_identity_name(config).await?;
    let skey = pijul_identity::Complete::load(&name)?.skey();

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    for c in to_upload {
        let CS::Change(hash) = c else { continue };
        let msg = format!("{} {} {}", state.to_base32(), hash.to_base32(), timestamp);
        let sig = pijul_identity::sign_sshsig_ns(&skey, "pijul-push", msg.as_bytes()).await?;
        let bytes = sig.to_bytes()?;
        remote
            .push_signature(channel, *hash, state, timestamp, &bytes)
            .await?;
    }
    Ok(())
}

impl Pull {
    /// Gets the `to_download` vec and calculates any remote unrecords.
    /// If the local remote cache can be auto-updated, it will be.
    async fn to_download<T: RawMutTxnT + 'static>(
        &self,
        txn: &ArcTxn<MutTxn<T>>,
        channel: &mut ChannelRef<MutTxn<T>>,
        repo: &mut Repository,
        remote: &mut RemoteRepo,
    ) -> Result<RemoteDelta<MutTxn<T>>, anyhow::Error> {
        let delta = remote
            .update_changelist_pushpull(
                txn,
                &self.path,
                channel,
                None,
                repo,
                self.changes.as_slice(),
                true,
            )
            .await?;
        let to_download = remote
            .pull(
                repo,
                txn,
                channel,
                delta.to_download.as_slice(),
                &delta.inodes,
                false,
            )
            .await?;

        Ok(RemoteDelta {
            to_download,
            ..delta
        })
    }

    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub async fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let mut repo = self.base.find_root()?;
        let txn = repo.pristine.arc_txn_begin()?;

        let txn_read = txn.read();
        let (channel_name, is_current_channel) =
            get_channel(self.to_channel.as_deref(), &*txn_read);
        let channel_name = channel_name.to_string();
        drop(txn_read);

        let mut touched_inodes = pijul_core::unrecord::TouchedInodes::new();

        let channel_sm: pijul_core::small_string::SmallString = channel_name.parse()?;
        let mut channel = txn.write().open_or_create_channel(&channel_sm)?;
        let remote_name = if let Some(ref rem) = self.from {
            rem
        } else if let Some(ref def) = config.default_remote {
            def
        } else {
            bail!("Missing remote")
        };
        let from_channel = self
            .from_channel
            .as_deref()
            .unwrap_or(pijul_core::DEFAULT_CHANNEL);
        let mut remote = remote::repository(
            config,
            Some(&repo.path),
            None,
            &remote_name,
            from_channel,
            self.no_cert_check,
            true,
        )
        .await?;
        debug!("downloading");

        let RemoteDelta {
            inodes,
            remote_ref,
            mut to_download,
            remote_unrecs,
            ..
        } = self
            .to_download(&txn, &mut channel, &mut repo, &mut remote)
            .await?;

        let hash = remote::pending(txn.clone(), &channel, &repo.working_copy, &repo.changes)?;

        if let Some(ref r) = remote_ref {
            remote.update_identities(&mut repo, r).await?;
        }

        if !self.no_notify {
            notify_remote_unrecords(&repo, remote_unrecs.as_slice());
        }

        if to_download.is_empty() {
            let mut stderr = std::io::stderr();
            writeln!(stderr, "Nothing to pull")?;
            if let Some(ref h) = hash {
                txn.write()
                    .unrecord(&repo.changes, &mut channel, h, 0, &mut touched_inodes)?;
            }
            txn.commit()?;
            return Ok(());
        }

        if self.changes.is_empty() {
            if !self.all {
                let mut o = make_changelist(&repo.changes, &to_download, "pull")?;
                to_download = loop {
                    let d = parse_changelist(&edit::edit_bytes(&o[..])?, &to_download);
                    let comp = complete_deps(&repo.changes, Some(&to_download), &d)?;
                    if comp.len() == d.len() {
                        break comp;
                    }
                    o = make_changelist(&repo.changes, &comp, "pull")?
                };
            }
        } else {
            to_download = complete_deps(&repo.changes, None, &to_download)?;
        }

        // Reverse of `unrecord_superseded`: if a LOCAL amend already superseded
        // an incoming change (its predecessor still lives on the remote we're
        // pulling), drop it from the download set — re-applying it would
        // order-conflict with the local amend (the amend-after-push trap). The
        // obsolescence markers were written by `record --amend`.
        {
            let txnr = txn.read();
            to_download.retain(|h| match h {
                CS::Change(c) => !txnr.is_superseded(c).unwrap_or(false),
                _ => true,
            });
        }

        // Honor `replaces`: before applying, supersede any amended predecessor
        // that a downloaded change replaces (mirrors `pijul apply`; without this,
        // an amend pulled in would stack on its predecessor and conflict).
        {
            let mut txnw = txn.write();
            for h in to_download.iter() {
                if let CS::Change(h) = h {
                    txnw.unrecord_superseded(&repo.changes, &channel, h)?;
                }
            }
        }

        {
            // Now that .pull is always given `false` for `do_apply`...
            let mut ws = pijul_core::ApplyWorkspace::new();
            debug!("to_download = {:#?}", to_download);
            let apply_bar = ProgressBar::new(to_download.len() as u64, APPLY_MESSAGE)?;

            let mut channel = channel.write();
            let mut txn = txn.write();
            let pins = config.pins.clone();
            for h in to_download.iter().rev() {
                match h {
                    CS::Change(h) => {
                        txn.apply_change_rec_ws(&repo.changes, &mut channel, h, &mut ws)?;
                        if !pins.is_empty() {
                            let change_id = *txn
                                .get_internal(&h.into())?
                                .expect("just-applied change not found in pristine");
                            pijul_core::pin::reorder_pinned(
                                &mut *txn,
                                &mut *channel,
                                change_id,
                                |h| pins.iter().any(|p| *p == h.to_base32()),
                            )
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        }
                    }
                    CS::State(s) => {
                        if let Some(n) = txn.channel_has_state(&channel.states, &s.into())? {
                            if !pins.is_empty()
                                && pijul_core::pin::state_includes_pinned(
                                    &*txn,
                                    &*channel,
                                    n.into(),
                                    |h| pins.iter().any(|p| *p == h.to_base32()),
                                )?
                            {
                                bail!(
                                    "Cannot tag state {}: it includes pinned patches",
                                    s.to_base32()
                                );
                            }
                            txn.put_tags(&mut channel.tags, n.into(), s)?;
                        } else {
                            bail!(
                                "Cannot add tag {}: channel {:?} does not have that state",
                                s.to_base32(),
                                channel.name
                            )
                        }
                    }
                }
                apply_bar.inc(1);
            }
        }

        debug!("completing changes");
        remote
            .complete_changes(&repo, &txn, &mut channel, &to_download, self.complete)
            .await?;
        remote.finish().await?;

        debug!("inodes = {:?}", inodes);
        debug!("to_download: {:?}", to_download.len());
        let mut touched = HashSet::new();
        let txn_ = txn.read();
        for d in to_download.iter() {
            debug!("to_download {:?}", d);
            match d {
                CS::Change(d) => {
                    if let Some(int) = txn_.get_internal(&d.into())? {
                        for inode in txn_.iter_rev_touched(int)? {
                            let (int_, inode) = inode?;
                            if int_ < int {
                                continue;
                            } else if int_ > int {
                                break;
                            }
                            let ext = pijul_core::pristine::Position {
                                change: txn_
                                    .get_external(&inode.change)
                                    .optional()?
                                    .unwrap()
                                    .into(),
                                pos: inode.pos,
                            };
                            if inodes.is_empty() || inodes.contains(&ext) {
                                touched.insert(*inode);
                            }
                        }
                    }
                }
                CS::State(_) => {
                    // No need to do anything for now here, we don't
                    // output after downloading a tag.
                }
            }
        }
        std::mem::drop(txn_);
        if is_current_channel {
            let mut touched_paths = BTreeSet::new();
            {
                let txn_ = txn.read();
                for &i in touched.iter() {
                    if let Some(pijul_core::fs::FindPath { path, .. }) = pijul_core::fs::find_path(
                        &repo.changes,
                        &*txn_,
                        &*channel.read(),
                        false,
                        i,
                    )? {
                        touched_paths.insert(path.join("/"));
                    } else {
                        // In this case, the change does touch paths,
                        // but we can't find the corresponding path
                        // name in the graph. This shouldn't happen,
                        // but if it does, re-outputting the entire
                        // repo is the right call.
                        touched_paths.clear();
                        touched_paths.insert(String::from(""));
                        break;
                    }
                }
            }
            let mut last: Option<&str> = None;
            let mut conflicts = Vec::new();
            let _output_spinner = Spinner::new(OUTPUT_MESSAGE);

            for path in touched_paths.iter() {
                match last {
                    Some(last_path) => {
                        // If `last_path` is a prefix (in the path sense) of `path`, skip.
                        if last_path.len() < path.len() {
                            let (pre_last, post_last) = path.split_at(last_path.len());
                            if pre_last == last_path && post_last.starts_with("/") {
                                continue;
                            }
                        }
                    }
                    _ => (),
                }
                debug!("path = {:?}", path);
                conflicts.extend(
                    pijul_core::output::output_repository_no_pending_current(
                        &repo.working_copy,
                        &repo.changes,
                        &txn,
                        &channel,
                        path,
                        true,
                        None,
                        std::thread::available_parallelism()?.get(),
                        0,
                        // Guarded by `is_current_channel` above.
                        true,
                    )?
                    .into_iter(),
                );
                last = Some(path)
            }

            if self.json {
                super::print_conflicts_json(&conflicts)?;
            } else {
                super::print_conflicts(&conflicts)?;
            }
        }
        if let Some(h) = hash {
            txn.write()
                .unrecord(&repo.changes, &mut channel, &h, 0, &mut touched_inodes)?;
            // The pending patch is ephemeral: drop its change file.
            repo.changes.del_change(&h)?;
        }

        txn.commit()?;
        Ok(())
    }
}

fn complete_deps<C: ChangeStore>(
    c: &C,
    original: Option<&[CS]>,
    now: &[CS],
) -> Result<Vec<CS>, anyhow::Error> {
    debug!("complete deps {:?} {:?}", original, now);
    let original_: Option<HashSet<_>> = original.map(|original| original.iter().collect());
    let now_: HashSet<_> = now.iter().cloned().collect();
    let mut result = Vec::with_capacity(original.unwrap_or(now).len());
    let mut result_h = HashSet::with_capacity(original.unwrap_or(now).len());
    let mut stack: Vec<_> = now.iter().rev().cloned().collect();
    while let Some(h) = stack.pop() {
        stack.push(h);
        let l0 = stack.len();
        let hh = if let CS::Change(h) = h {
            h
        } else {
            stack.pop();
            result.push(h);
            continue;
        };
        for d in c.get_dependencies(&hh)? {
            let is_missing =
                now_.get(&CS::Change(d)).is_none() && result_h.get(&CS::Change(d)).is_none();

            debug!("complete_deps {:?} {:?}", d, is_missing);
            let is_missing = if let Some(ref original) = original_ {
                // If this is a list we submitted to the user for editing
                original.get(&CS::Change(d)).is_some() && is_missing
            } else {
                // Else, we were given an explicit list of patches to pull/push
                is_missing
            };
            if is_missing {
                // The user missed a dep.
                stack.push(CS::Change(d));
            }
        }
        if stack.len() == l0 {
            // We have all dependencies.
            stack.pop();
            debug!("all deps, push");
            if result_h.insert(h) {
                result.push(h);
            }
        }
    }
    debug!("result {:?}", result);
    Ok(result)
}

fn check_deps<C: ChangeStore>(c: &C, original: &[CS], now: &[CS]) -> Result<(), anyhow::Error> {
    let original_: HashSet<_> = original.iter().collect();
    let now_: HashSet<_> = now.iter().collect();
    for n in now {
        // check that all of `now`'s deps are in now or not in original
        let n = if let CS::Change(n) = n { n } else { continue };
        for d in c.get_dependencies(n)? {
            if original_.get(&CS::Change(d)).is_some() && now_.get(&CS::Change(d)).is_none() {
                bail!("Missing dependency: {:?}", n)
            }
        }
    }
    Ok(())
}

fn notify_remote_unrecords(repo: &Repository, remote_unrecs: &[(u64, pijul_remote::CS)]) {
    use std::fmt::Write;
    if !remote_unrecs.is_empty() {
        let mut s = format!(
            "# The following changes have been unrecorded in the remote.\n\
            # This buffer is only being used to inform you of the remote change;\n\
            # your push will continue when it is closed.\n"
        );
        for (_, hash) in remote_unrecs {
            let header = match hash {
                CS::Change(hash) => repo.changes.get_header(hash).unwrap(),
                CS::State(_) => pijul_core::change::ChangeHeader::default(),
            };
            s.push_str("#\n");
            writeln!(&mut s, "#    {}", header.message).unwrap();
            writeln!(&mut s, "#    {}", header.timestamp).unwrap();
            match hash {
                CS::Change(hash) => {
                    writeln!(&mut s, "#    {}", hash.to_base32()).unwrap();
                }
                CS::State(hash) => {
                    writeln!(&mut s, "#    {}", hash.to_base32()).unwrap();
                }
            }
        }
        if let Err(e) = edit::edit(s.as_str()) {
            log::error!(
                "Notification of remote unrecords experienced an error: {}",
                e
            );
        }
    }
}

fn notify_unknown_changes(unknown_changes: &[pijul_remote::CS]) {
    use std::fmt::Write;
    if unknown_changes.is_empty() {
        return;
    } else {
        let mut s = format!(
            "# The following changes are new in the remote\n# (and are not yet known to your local copy):\n#\n"
        );
        let rest_len = unknown_changes.len().saturating_sub(5);
        for hash in unknown_changes.iter().take(5) {
            let hash = match hash {
                CS::Change(hash) => hash.to_base32(),
                CS::State(hash) => hash.to_base32(),
            };
            writeln!(&mut s, "#     {}", hash).expect("Infallible write to String");
        }
        if rest_len > 0 {
            let plural = if rest_len == 1 { "" } else { "s" };
            writeln!(&mut s, "#     ... plus {} more change{}", rest_len, plural)
                .expect("Infallible write to String");
        }
        if let Err(e) = edit::edit(s.as_str()) {
            log::error!(
                "Notification of unknown changes experienced an error: {}",
                e
            );
        }
    }
}
