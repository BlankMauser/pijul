use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use canonical_path::CanonicalPathBuf;
use clap::Parser;
use jiff::Timestamp;
use log::debug;
use pijul_core::change::*;
use pijul_core::changestore::*;
use pijul_core::{
    ArcTxn, Base32, ChangeId, ChannelMutTxnT, ChannelRef, ChannelTxnT, GraphTxnT, MutTxnTExt, TxnT,
    TxnTExt,
};
use pijul_core::{HashMap, HashSet};

use crate::commands::common_opts::{DiffAlgorithm, RepoAndChannel};
use crate::commands::load_channel;

#[derive(Parser, Debug)]
pub struct Record {
    #[clap(flatten)]
    base: RepoAndChannel,
    /// Record all paths that have changed
    #[clap(short = 'a', long = "all")]
    pub all: bool,
    /// When the change touches more than one sub-root (imported project),
    /// split it into one commuting change per sub-root instead of erroring.
    #[clap(long = "split-per-root", conflicts_with_all = ["amend", "force"])]
    pub split_per_root: bool,
    /// Allow a single change to span more than one sub-root (imported project)
    /// instead of erroring. The change will not commute across projects.
    #[clap(long = "force")]
    pub force: bool,
    /// Record files that still contain conflict markers (`>>>>>>>` / `=======` /
    /// `<<<<<<<`) as literal content. By default such a record is refused, since
    /// the markers are almost always an unresolved conflict rather than intended
    /// text.
    #[clap(long = "accept-conflict-markers")]
    pub accept_conflict_markers: bool,
    /// Set the change message
    #[clap(short = 'm', long = "message")]
    pub message: Option<String>,
    /// Set the description field.
    #[clap(long = "description")]
    pub description: Option<String>,
    /// Set the author field
    #[clap(long = "author")]
    pub author: Option<String>,
    /// Set the timestamp field
    #[clap(long = "timestamp")]
    pub timestamp: Option<Timestamp>,
    /// Amend this change instead of creating a new change
    #[clap(long = "amend")]
    #[allow(clippy::option_option)]
    pub amend: Option<Option<String>>,
    /// Paths in which to record the changes
    pub prefixes: Vec<PathBuf>,
    /// Identity to sign changes with
    #[clap(long = "identity")]
    pub identity: Option<String>,
    /// Diff algorithm to use (default: myers)
    #[clap(long = "algorithm", value_enum)]
    pub algorithm: Option<DiffAlgorithm>,
    /// Record the change described by FILE instead of opening an editor. FILE
    /// uses the same text format as `pijul diff` and the interactive editor,
    /// so a caller can `pijul diff`, drop the hunks it does not want, and
    /// record the remainder non-interactively.
    #[clap(long = "from-change", value_name = "FILE", conflicts_with = "all")]
    pub from_change: Option<PathBuf>,
}

/// Split a Git-style "subject\n\nbody" message into (subject, Some(body)).
/// Without a blank-line separator the whole string is the subject (no body).
fn split_message(msg: &str) -> (String, Option<String>) {
    match msg.split_once("\n\n") {
        Some((subject, body)) if !body.trim().is_empty() => (
            subject.trim_end().to_string(),
            Some(body.trim().to_string()),
        ),
        _ => (msg.to_string(), None),
    }
}

impl Record {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub async fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let mut repo = self.base.find_root()?;
        let mut stdout = std::io::stdout();
        let mut stderr = std::io::stderr();
        // Shared hooks (from the repo's tracked `pijul.toml`) are versioned code
        // shared with every author, so they only run once their current content
        // has been approved with `pijul hooks approve`; otherwise we skip them
        // with a warning. Personal hooks (from `.pijul/config.toml`) are always
        // yours to run. Execution order: every `preHooks` first (the escape
        // hatch to run ahead of the shared list), then the shared `record`
        // hooks, then the personal `record` hooks.
        let shared_ok = config.shared_hooks_approved();
        if !shared_ok {
            writeln!(
                stderr,
                "warning: shared hooks in {} are not approved and will be skipped; run `pijul hooks approve` to enable them",
                pijul_config::SHARED_CONFIG_FILE,
            )?;
        }
        let mut hooks: Vec<&pijul_config::hook::HookEntry> = Vec::new();
        hooks.extend(config.hooks.pre.iter());
        if shared_ok {
            hooks.extend(config.shared_hooks.pre.iter());
            hooks.extend(config.shared_hooks.record.iter());
        }
        hooks.extend(config.hooks.record.iter());

        // Per-file hooks (those mentioning `$FILE`) run once per changed file
        // instead of once for the whole repo. Computing that list needs a
        // read-only pass over the working copy, so only do it when some hook
        // actually asks for it.
        let changed_files = if hooks.iter().any(|h| h.is_per_file()) {
            self.changed_files(&repo)?
        } else {
            Vec::new()
        };
        for h in hooks {
            if h.is_per_file() {
                for f in &changed_files {
                    h.run(repo.path.clone(), Some(f))?
                }
            } else {
                h.run(repo.path.clone(), None)?
            }
        }
        let txn = repo.pristine.arc_txn_begin()?;
        let mut touched_inodes = pijul_core::unrecord::TouchedInodes::new();

        let (mut channel, extra) = {
            let txn = txn.read();

            let (channel, _) = load_channel(self.base.channel(), &*txn)?;

            let mut extra = Vec::new();
            for h in config.extra_dependencies.iter() {
                let (h, c) = txn.hash_from_prefix(h)?;
                if txn
                    .get_changeset(txn.changes(&*channel.read()), &c)?
                    .is_none()
                {
                    bail!(
                        "Change {:?} (from .pijul/config) is not on channel {:?}",
                        h,
                        channel.read().name
                    )
                }
                extra.push(h)
            }

            (channel, extra)
        };

        // The change being amended, if any — recorded into the new change's
        // lineage metadata (`replaces`/`root`) below so a Nest can supersede it.
        let mut amend_of: Option<pijul_core::pristine::Hash> = None;
        let header = if let Some(ref amend) = self.amend {
            let h = {
                let txn = txn.read();

                if let Some(hash) = amend {
                    txn.hash_from_prefix(hash)?.0
                } else if let Some(h) = txn.reverse_log(&*channel.read(), None)?.next() {
                    h?.1.0.into()
                } else {
                    return Ok(());
                }
            };
            amend_of = Some(h);
            let header = if let Some(message) = self.message.clone() {
                ChangeHeader {
                    message,
                    ..repo.changes.get_header(&h)?
                }
            } else {
                repo.changes.get_header(&h)?
            };

            let salt = self.timestamp.map(Timestamp::as_second).unwrap_or(0) as u64;
            let mut txn = txn.write();
            // Drop the amended predecessor from the channel. This also drops the
            // markers it owned, but that is harmless: the new amend, recorded
            // below, carries the predecessor's whole chain plus the predecessor
            // itself, and its `unrecord_superseded` re-marks all of them — so the
            // lineage stays filtered without any special "superseding" unrecord.
            txn.unrecord(&repo.changes, &mut channel, &h, salt, &mut touched_inodes)?;
            txn.touch_inodes(&mut repo.working_copy, &mut touched_inodes)?;
            header
        } else {
            self.header(config).await?
        };
        let no_prefixes = self.prefixes.is_empty();
        let split_per_root = self.split_per_root;
        let force = self.force;
        let repo_path = CanonicalPathBuf::canonicalize(&repo.path)?;

        let complete =
            pijul_identity::Complete::load(&pijul_identity::choose_identity_name(config).await?)?;

        let secret = complete.skey();

        txn.write()
            .apply_root_change_if_needed(&repo.changes, &channel, rand::rng())?;

        let (stat_updates, result) = self.record(
            txn.clone(),
            channel.clone(),
            &repo.working_copy,
            &repo.changes,
            repo_path,
            header,
            &extra,
            config.boundaries(),
        )?;
        match result {
            Some((mut change, updates, oldest, actions_before_edit)) => {
                // How many distinct sub-roots (imported projects) does this
                // change touch? Recording several at once produces a change
                // that cannot commute across projects, so it is gated behind
                // an explicit choice (see below).
                let groups = {
                    let txn_ = txn.read();
                    pijul_core::record::group_by_sub_root(
                        &*txn_,
                        &*channel.read(),
                        &change.hashed.changes,
                        &updates,
                    )?
                };
                if groups.len() > 1 && !split_per_root && !force {
                    let mut names = String::new();
                    {
                        let txn_ = txn.read();
                        let channel_ = channel.read();
                        for (sr, _) in groups.iter() {
                            match sr {
                                pijul_core::record::SubRoot::Existing(pos) => {
                                    let relocated = pijul_core::pristine::is_relocated_sub_root(
                                        &*txn_,
                                        txn_.graph(&*channel_),
                                        *pos,
                                    )
                                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                                    let path = pijul_core::pristine::path_of_sub_root(&*txn_, *pos)
                                        .map_err(|e| anyhow::anyhow!("{}", e))?
                                        .filter(|p| !p.is_empty());
                                    match (relocated, path) {
                                        (true, Some(p)) => names
                                            .push_str(&format!("\n  - {} (imported project)", p)),
                                        (true, None) => names.push_str("\n  - (imported project)"),
                                        (false, Some(p)) => names.push_str(&format!("\n  - {}", p)),
                                        (false, None) => names.push_str("\n  - . (main project)"),
                                    }
                                }
                                pijul_core::record::SubRoot::New => {
                                    names.push_str("\n  - (new project)")
                                }
                            }
                        }
                    }
                    bail!(
                        "This record touches {} independent roots:{}\n\
                         A single change may not commute across projects.\n\
                         Re-run with --split-per-root to record one commuting change per root,\n\
                         or --force to record them together as a single (non-commuting) change.",
                        groups.len(),
                        names,
                    );
                }
                if groups.len() > 1 && split_per_root {
                    // One commuting change per sub-root. `apply_local_change`
                    // only reads the *values* of the inode-update map (never
                    // the keys), and those reference positions in the shared
                    // `contents` buffer, so each sub-change keeps the full
                    // buffer and receives only the updates for its own hunks.
                    //
                    // Partition the inode-updates by *hunk index*, reusing the
                    // grouping `group_by_sub_root` already computed. An
                    // `InodeUpdate` with key `K` always describes the hunk at
                    // index `K - 1`: both insertion sites key it on
                    // `actions.len()` (`Add`, just after its hunk is pushed) or
                    // `actions.len() + 1` (`Deleted`, just before the deletion
                    // hunk is pushed), so `key == hunk_index + 1` in either
                    // case. Re-deriving the sub-root from the inode (via
                    // `inode_sub_root`) instead can disagree with the hunk
                    // grouping for nested/relocated sub-roots and route both
                    // updates into one group's change — leaving the other
                    // file's inode unmapped, so the next record reintroduces it
                    // (a spurious duplicate that then conflicts on its name).
                    let mut split_changes = Vec::new();
                    {
                        let txn_ = txn.read();
                        for (_sr, indices) in groups.iter() {
                            let index_set: std::collections::HashSet<usize> =
                                indices.iter().copied().collect();
                            let hunks: Vec<_> = indices
                                .iter()
                                .map(|&i| change.hashed.changes[i].clone())
                                .collect();
                            let c = pijul_core::change::LocalChange::make_change(
                                &*txn_,
                                &channel,
                                hunks,
                                change.contents.clone(),
                                change.hashed.header.clone(),
                                Vec::new(),
                            )?;
                            let ups: HashMap<usize, pijul_core::InodeUpdate> = updates
                                .iter()
                                .filter(|(k, _)| {
                                    k.checked_sub(1).map_or(false, |h| index_set.contains(&h))
                                })
                                .map(|(k, u)| (*k, u.clone()))
                                .collect();
                            split_changes.push((c, ups));
                        }
                    }

                    let mut txn_ = txn.write();
                    for (mut c, ups) in split_changes {
                        if c.hashed.header.message.is_empty() {
                            continue;
                        }
                        let hash_for_sig = c.hash()?;
                        let sig_pem =
                            pijul_identity::sign_pem(&secret, &hash_for_sig.to_bytes()).await?;
                        c.unhashed = Some(serde_json::json!({ "signature": sig_pem }));
                        let hash = repo
                            .changes
                            .save_change(&mut c, |_, _| Ok::<_, anyhow::Error>(()))?;
                        txn_.apply_local_change(&mut channel, &c, &hash, &ups)?;
                        writeln!(stdout, "Hash: {}", hash.to_base32())?;
                    }
                    pijul_core::record::update_stat_cache(&mut *txn_, &stat_updates, false)
                        .map_err(|e| anyhow::anyhow!("stat cache: {:?}", e))?;

                    let mut path = repo.path.join(pijul_core::DOT_DIR);
                    path.push("identities");
                    std::fs::create_dir_all(&path)?;

                    if no_prefixes {
                        let mut oldest = oldest
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        if oldest == 0 {
                            oldest = std::time::SystemTime::now()
                                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64;
                        }
                        txn_.touch_channel(&mut *channel.write(), Some(oldest + 1));
                        // The set of touched inodes is the same as the
                        // un-split change; use its atoms to decide which
                        // working-copy files to refresh.
                        let mut actions_after_edit = HashMap::new();
                        for i in change.hashed.changes.iter() {
                            for i in i.iter() {
                                match i {
                                    Atom::NewVertex(n) => {
                                        actions_after_edit
                                            .entry(n.inode)
                                            .or_insert(HashSet::new())
                                            .insert(Atom::NewVertex(n.clone()));
                                    }
                                    Atom::EdgeMap(e) => {
                                        actions_after_edit
                                            .entry(e.inode)
                                            .or_insert(HashSet::new())
                                            .insert(Atom::EdgeMap(e.clone()));
                                    }
                                }
                            }
                        }
                        let mut touched = pijul_core::unrecord::TouchedInodes::new();
                        for (i, x) in actions_before_edit.iter() {
                            if let Some(y) = actions_after_edit.get(i) {
                                if x != y {
                                    touched.insert((ChangeId::ROOT, *i));
                                }
                            } else {
                                touched.insert((ChangeId::ROOT, *i));
                            }
                        }
                        txn_.touch_inodes(&mut repo.working_copy, &touched)?;
                    }
                    std::mem::drop(txn_);
                    txn.commit()?;
                    return Ok(());
                }

                // Stamp lineage before hashing: the amend chain is the amended
                // change's own chain, extended with the amended change itself
                // (now superseded too). `root` is derived as the chain's first
                // element. Carrying the *whole* chain — computed here, where the
                // full local lineage is visible — lets any peer supersede every
                // iteration and group revisions even when it never received the
                // intermediate ones.
                if let Some(replaced) = amend_of {
                    let mut chain = repo
                        .changes
                        .get_change(&replaced)
                        .ok()
                        .map(|c| c.replaces_chain())
                        .unwrap_or_default();
                    chain.push(replaced);
                    change.hashed.set_change_metadata(&chain);
                }
                let hash_for_sig = change.hash()?;
                let sig_pem = pijul_identity::sign_pem(&secret, &hash_for_sig.to_bytes()).await?;
                change.unhashed = Some(serde_json::json!({ "signature": sig_pem }));
                let hash = repo
                    .changes
                    .save_change(&mut change, |_, _| Ok::<_, anyhow::Error>(()))?;

                let mut txn_ = txn.write();
                if !change.hashed.header.message.is_empty() {
                    txn_.apply_local_change(&mut channel, &change, &hash, &updates)?;
                    // The inodes now exist with their final positions: persist the
                    // working-copy (mtime, size) cache so the next `record` skips
                    // these files. See notes-record-stat-cache.md.
                    pijul_core::record::update_stat_cache(&mut *txn_, &stat_updates, false)
                        .map_err(|e| anyhow::anyhow!("stat cache: {:?}", e))?;

                    // Record the obsolescence marker "predecessor superseded by
                    // this amend" through the single core write path, so a later
                    // pull won't re-introduce the predecessor and conflict with
                    // this change (amend-after-push). The predecessor is already
                    // gone from the channel (unrecorded above), so this only
                    // writes the marker.
                    if amend_of.is_some() {
                        txn_.unrecord_superseded(&repo.changes, &channel, &hash)?;
                    }

                    if !config.pins.is_empty() {
                        let change_id = *txn_
                            .get_internal(&(&hash).into())?
                            .expect("just-applied change not found in pristine");
                        let pins = config.pins.clone();
                        let mut ch = channel.write();
                        pijul_core::pin::reorder_pinned(&mut *txn_, &mut *ch, change_id, |h| {
                            pins.iter().any(|p| *p == h.to_base32())
                        })
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    }

                    let mut path = repo.path.join(pijul_core::DOT_DIR);
                    path.push("identities");
                    std::fs::create_dir_all(&path)?;
                    writeln!(stdout, "Hash: {}", hash.to_base32())?;
                }

                debug!("oldest = {:?}", oldest);
                if no_prefixes {
                    let mut oldest = oldest
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    if oldest == 0 {
                        // If no diff was done at all, it means that no
                        // existing file changed since last time (some
                        // files may have been added, deleted or moved,
                        // but `touch` isn't about those).
                        oldest = std::time::SystemTime::now()
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                    }
                    txn_.touch_channel(&mut *channel.write(), Some(oldest + 1));
                    let mut actions_after_edit = HashMap::new();
                    for i in change.hashed.changes.iter() {
                        for i in i.iter() {
                            match i {
                                Atom::NewVertex(n) => {
                                    actions_after_edit
                                        .entry(n.inode)
                                        .or_insert(HashSet::new())
                                        .insert(Atom::NewVertex(n.clone()));
                                }
                                Atom::EdgeMap(e) => {
                                    actions_after_edit
                                        .entry(e.inode)
                                        .or_insert(HashSet::new())
                                        .insert(Atom::EdgeMap(e.clone()));
                                }
                            }
                        }
                    }

                    let mut touched = pijul_core::unrecord::TouchedInodes::new();
                    for (i, x) in actions_before_edit.iter() {
                        if let Some(y) = actions_after_edit.get(i) {
                            if x != y {
                                touched.insert((ChangeId::ROOT, *i));
                            }
                        } else {
                            touched.insert((ChangeId::ROOT, *i));
                        }
                    }
                    debug!("touch {:?}", touched);
                    txn_.touch_inodes(&mut repo.working_copy, &touched)?;
                }

                std::mem::drop(txn_);

                txn.commit()?;
            }
            None => {
                // Nothing to record, but the walk still confirmed every clean
                // file's (mtime, size). Persist that so the next `record` skips
                // them instead of re-diffing the whole tree every time. The
                // inodes already exist with final positions (no apply needed).
                let mut txn_ = txn.write();
                pijul_core::record::update_stat_cache(&mut *txn_, &stat_updates, false)
                    .map_err(|e| anyhow::anyhow!("stat cache: {:?}", e))?;
                if no_prefixes {
                    txn_.touch_channel(&mut *channel.write(), None);
                }
                std::mem::drop(txn_);
                txn.commit()?;
                writeln!(stderr, "Nothing to record")?;
            }
        }
        Ok(())
    }

    async fn header(&self, config: &pijul_config::Config) -> Result<ChangeHeader, anyhow::Error> {
        let mut authors = Vec::new();
        let mut b = std::collections::BTreeMap::new();
        if let Some(ref a) = self.author {
            b.insert("name".to_string(), a.clone());
        } else {
            let identity_name = self
                .identity
                .clone()
                .unwrap_or(pijul_identity::choose_identity_name(config).await?);

            let public_key = pijul_identity::public_key(&identity_name);
            b.insert("key".to_string(), public_key?.key);
        }

        authors.push(Author(b));
        let templates = config.template.as_ref();
        let message = if let Some(message) = &self.message {
            message.clone()
        } else if let Some(message_file) = templates.and_then(|t| t.message.as_ref()) {
            match std::fs::read_to_string(message_file) {
                Ok(m) => m,
                Err(e) => bail!("Could not read message template: {:?}: {}", message_file, e),
            }
        } else {
            String::new()
        };
        let description = if let Some(description) = &self.description {
            Some(description.clone())
        } else if let Some(descr_file) = templates.and_then(|t| t.description.as_ref()) {
            match std::fs::read_to_string(descr_file) {
                Ok(d) => Some(d),
                Err(e) => bail!(
                    "Could not read description template: {:?}: {}",
                    descr_file,
                    e
                ),
            }
        } else {
            None
        };
        // Git-style convenience: a "subject\n\nbody" message with no explicit
        // --description splits into message (subject) + description (body).
        // Mirrors `git commit -m` and how agents tend to write messages.
        // NOTE: this changes `-m` semantics for multi-paragraph messages.
        let (message, description) = if description.is_none() {
            split_message(&message)
        } else {
            (message, description)
        };
        let header = ChangeHeader {
            message,
            authors,
            description,
            timestamp: self.timestamp.unwrap_or_else(Timestamp::now),
        };
        Ok(header)
    }

    fn fill_relative_prefixes(&mut self) -> Result<(), anyhow::Error> {
        let cwd = std::env::current_dir()?;
        for p in self.prefixes.iter_mut() {
            if p.is_relative() {
                *p = cwd.join(&p);
            }
        }
        Ok(())
    }

    /// Read-only pass over the working copy returning the repo-relative paths
    /// of files with unrecorded changes (adds, edits, moves, restores — not
    /// deletes, whose file is gone). Feeds per-file `$FILE` hooks; deliberately
    /// does not apply or cache anything.
    fn changed_files(
        &mut self,
        repo: &pijul_repository::Repository,
    ) -> Result<Vec<String>, anyhow::Error> {
        let txn = repo.pristine.arc_txn_begin()?;
        let (channel, _) = load_channel(self.base.channel(), &*txn.read())?;
        let mut state = pijul_core::RecordBuilder::new();
        if self.prefixes.is_empty() {
            state.record(
                txn.clone(),
                pijul_core::Algorithm::default(),
                false,
                &pijul_core::DEFAULT_SEPARATOR,
                channel.clone(),
                &repo.working_copy,
                &repo.changes,
                "",
                std::thread::available_parallelism()?.get(),
            )?;
        } else {
            self.fill_relative_prefixes()?;
            repo.working_copy.record_prefixes(
                txn.clone(),
                pijul_core::Algorithm::default(),
                channel.clone(),
                &repo.changes,
                &mut state,
                CanonicalPathBuf::canonicalize(&repo.path)?,
                &self.prefixes,
                false,
                std::thread::available_parallelism()?.get(),
                0,
            )?;
        }
        let rec = state.finish();
        let actions: Vec<_> = {
            let txn_ = txn.read();
            rec.actions
                .into_iter()
                .map(|r| r.globalize(&*txn_).unwrap())
                .collect()
        };
        let contents = if let Ok(c) = Arc::try_unwrap(rec.contents) {
            c.into_inner()
        } else {
            unreachable!()
        };
        let change = LocalChange::make_change(
            &*txn.read(),
            &channel,
            actions,
            contents,
            ChangeHeader::default(),
            Vec::new(),
        )?;
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        for hunk in change.changes.iter() {
            let path = match hunk {
                Hunk::Edit {
                    local: Local { path, .. },
                    ..
                }
                | Hunk::Replacement {
                    local: Local { path, .. },
                    ..
                } => path,
                Hunk::FileAdd { path, .. }
                | Hunk::FileUndel { path, .. }
                | Hunk::FileMove { path, .. } => path,
                _ => continue,
            };
            if seen.insert(path.clone()) {
                files.push(path.clone());
            }
        }
        Ok(files)
    }

    fn record<
        T: TxnTExt + MutTxnTExt + Sync + Send + 'static,
        C: ChangeStore + Send + Clone + 'static,
    >(
        mut self,
        txn: ArcTxn<T>,
        channel: ChannelRef<T>,
        working_copy: &pijul_core::working_copy::FileSystem,
        changes: &C,
        repo_path: CanonicalPathBuf,
        header: ChangeHeader,
        extra_deps: &[pijul_core::Hash],
        boundaries: &[String],
    ) -> Result<
        (
            // Per-inode (mtime, size, clean) observed during the walk. Always
            // returned — even with nothing to record — so the caller can warm
            // the stat cache and skip these files next time.
            Vec<(pijul_core::pristine::Inode, u64, u64, bool)>,
            Option<(
                Change,
                HashMap<usize, pijul_core::InodeUpdate>,
                std::time::SystemTime,
                HashMap<
                    pijul_core::pristine::Position<Option<pijul_core::Hash>>,
                    HashSet<pijul_core::change::Atom<Option<pijul_core::Hash>>>,
                >,
            )>,
        ),
        anyhow::Error,
    > {
        let mut state = pijul_core::RecordBuilder::new();
        // Monorepo boundaries (from the tracked `pijul.toml`): a `FileMove`
        // crossing one is collected during the walk and refused below unless
        // `--force`.
        state.set_boundaries(boundaries.to_vec());
        let algorithm = match self.algorithm {
            Some(DiffAlgorithm::Patience) => pijul_core::Algorithm::Patience,
            Some(DiffAlgorithm::Histogram) => pijul_core::Algorithm::ImaraHistogram,
            None => pijul_core::Algorithm::default(),
        };
        if self.prefixes.is_empty() {
            state.record(
                txn.clone(),
                algorithm,
                false,
                &pijul_core::DEFAULT_SEPARATOR,
                channel.clone(),
                working_copy,
                changes,
                "",
                std::thread::available_parallelism()?.get(),
            )?
        } else {
            self.fill_relative_prefixes()?;
            working_copy.record_prefixes(
                txn.clone(),
                algorithm,
                channel.clone(),
                changes,
                &mut state,
                repo_path,
                &self.prefixes,
                false,
                std::thread::available_parallelism()?.get(),
                self.timestamp.map(Timestamp::as_second).unwrap_or(0) as u64,
            )?;
        }

        let mut rec = state.finish();
        // Per-inode (mtime, size) of files confirmed clean during the walk;
        // written to the stat cache after the change is applied (see `run`).
        let stat_updates = rec.take_stat_updates();
        if rec.actions.is_empty() {
            return Ok((stat_updates, None));
        }

        // Monorepo boundary guard: a boundary-crossing move is the one
        // non-splittable operation that couples two projects (it reparents a
        // NAME vertex across the boundary), so refuse it unless `--force`. A
        // multi-boundary record WITHOUT such a move stays splittable and is not
        // blocked here (see `--split-per-root`).
        if !self.force && !rec.boundary_crossings.is_empty() {
            let mut list = String::new();
            for (old, new) in &rec.boundary_crossings {
                list.push_str(&format!("\n  - {} -> {}", old, new));
            }
            bail!(
                "This record moves {} file(s) across a monorepo boundary:{}\n\
                 A boundary-crossing move couples two projects and breaks their separability.\n\
                 Re-run with --force to record it anyway (and update `boundaries` in \
                 pijul.toml if a boundary root itself moved).",
                rec.boundary_crossings.len(),
                list,
            );
        }

        // Conflict-marker guard: recording a file whose content still holds
        // `>>>>>>>` / `=======` / `<<<<<<<` markers is almost always a conflict
        // left unresolved (a live conflict is handled as structure and never
        // reaches content; markers reach content only once orphaned). Refuse it
        // unless the user explicitly opts in.
        if !self.accept_conflict_markers && !rec.conflict_marker_files.is_empty() {
            let mut list = String::new();
            for path in &rec.conflict_marker_files {
                list.push_str(&format!("\n  - {}", path));
            }
            bail!(
                "This record would commit conflict markers as content in {} file(s):{}\n\
                 These files still contain `>>>>>>>` / `=======` / `<<<<<<<` markers.\n\
                 Resolve the conflict (edit the file and remove the markers), or re-run \
                 with --accept-conflict-markers to record them verbatim.",
                rec.conflict_marker_files.len(),
                list,
            );
        }

        if rec.has_binary_files && !self.all {
            bail!("Cannot record a binary change interactively. Please use -a.")
        }

        debug!("TAKING LOCK {}", line!());
        let txn_ = txn.write();
        let actions: Vec<_> = rec
            .actions
            .into_iter()
            .map(|rec| rec.globalize(&*txn_).unwrap())
            .collect();

        let mut actions_before_edit = HashMap::new();
        for i in actions.iter() {
            for i in i.iter() {
                match i {
                    Atom::NewVertex(n) => {
                        actions_before_edit
                            .entry(n.inode)
                            .or_insert(HashSet::new())
                            .insert(Atom::NewVertex(n.clone()));
                    }
                    Atom::EdgeMap(e) => {
                        actions_before_edit
                            .entry(e.inode)
                            .or_insert(HashSet::new())
                            .insert(Atom::EdgeMap(e.clone()));
                    }
                }
            }
        }

        debug!("globalize done");
        let contents = if let Ok(c) = Arc::try_unwrap(rec.contents) {
            c.into_inner()
        } else {
            unreachable!()
        };
        let mut change = LocalChange::make_change(
            &*txn_,
            &channel,
            actions,
            contents,
            header.clone(),
            Vec::new(),
        )?;
        debug!("make change done");
        let current: HashSet<_> = change.dependencies.iter().cloned().collect();
        for dep in extra_deps.iter() {
            if !current.contains(dep) {
                change.dependencies.push(*dep)
            }
        }

        debug!("has_binary = {:?}", rec.has_binary_files);
        let mut change = if self.all {
            change
        } else if let Some(ref from) = self.from_change {
            // Non-interactive review: read the (already pruned) change from a
            // file and parse it exactly as the editor result would be parsed.
            // Same text format as `pijul diff`, so a caller can `pijul diff`,
            // drop the hunks it doesn't want, and record the remainder.
            let bytes = std::fs::read(from)?;
            if bytes.iter().all(|c| (*c as char).is_whitespace()) {
                bail!("Empty change")
            }
            let mut reader = std::io::BufReader::new(std::io::Cursor::new(&bytes));
            let mut change =
                Change::read_and_deps(&mut reader, &mut rec.updatables, &*txn_, &channel).map_err(
                    |e| anyhow::anyhow!("Cannot parse change from {}: {}", from.display(), e),
                )?;
            if change.changes.is_empty() {
                bail!("Cannot parse change")
            }
            // The author emitted by `pijul diff` is a placeholder; the real
            // identity is authoritative. Keep the message/description the caller
            // wrote in the file (e.g. edited it), but let -m / --description win.
            change.hashed.header.authors = header.authors;
            if let Some(ref m) = self.message {
                change.hashed.header.message = m.clone();
            }
            if let Some(ref d) = self.description {
                change.hashed.header.description = Some(d.clone());
            }
            change
        } else {
            let mut o = Vec::new();
            debug!("write change");
            change.write(changes, None, true, &mut o)?;
            debug!("write change done");

            let mut with_errors: Option<Vec<u8>> = None;
            let change = loop {
                let mut bytes = if let Some(ref o) = with_errors {
                    edit::edit_bytes_with_builder(
                        &o[..],
                        tempfile::Builder::new().suffix(".pijul-commit"),
                    )?
                } else {
                    edit::edit_bytes_with_builder(
                        &o[..],
                        tempfile::Builder::new().suffix(".pijul-commit"),
                    )?
                };
                if bytes.iter().all(|c| (*c as char).is_whitespace()) {
                    bail!("Empty change")
                }
                let mut change = std::io::BufReader::new(std::io::Cursor::new(&bytes));
                if let Ok(change) =
                    Change::read_and_deps(&mut change, &mut rec.updatables, &*txn_, &channel)
                {
                    break change;
                }

                let mut err = SYNTAX_ERROR.as_bytes().to_vec();
                err.append(&mut bytes);
                with_errors = Some(err)
            };
            if change.changes.is_empty() {
                bail!("Cannot parse change")
            }
            change
        };

        let mut actions_after_edit = HashMap::new();
        for i in change.hashed.changes.iter() {
            for i in i.iter() {
                match i {
                    Atom::NewVertex(n) => {
                        actions_after_edit
                            .entry(n.inode)
                            .or_insert(HashSet::new())
                            .insert(Atom::NewVertex(n.clone()));
                    }
                    Atom::EdgeMap(e) => {
                        actions_after_edit
                            .entry(e.inode)
                            .or_insert(HashSet::new())
                            .insert(Atom::EdgeMap(e.clone()));
                    }
                }
            }
        }

        let current: HashSet<_> = change.dependencies.iter().cloned().collect();
        for dep in extra_deps.iter() {
            if !current.contains(dep) {
                change.dependencies.push(*dep)
            }
        }

        Ok((
            stat_updates,
            Some((
                change,
                rec.updatables,
                rec.oldest_change,
                actions_before_edit,
            )),
        ))
    }
}

const SYNTAX_ERROR: &str = "# Syntax errors, please try again.
# Alternatively, you may delete the entire file (including this
# comment) to abort.
";
