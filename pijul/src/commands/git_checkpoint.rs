//! Local-only, versioned replay recipes for `pijul git`'s discarded channels.
//! No recorder/apply optimizations live here. A recipe describes the actual
//! ordered native log, including merge-parent applications and no-op commits.
use anyhow::{Context, bail};
use pijul_core::pristine::*;
use pijul_core::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

const MAX_BYTES: u64 = 64 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    first_parent: Option<String>,
    append: Vec<String>,
    state: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Checkpoints {
    schema: u32,
    pub(super) head: String,
    pub(super) converted: usize,
    commits: BTreeMap<String, Recipe>,
}

impl Checkpoints {
    pub(super) fn new() -> Self {
        Self {
            schema: 1,
            head: String::new(),
            converted: 0,
            commits: BTreeMap::new(),
        }
    }

    pub(super) fn read(path: &Path) -> Result<Self, anyhow::Error> {
        let meta = path.symlink_metadata()?;
        if !meta.is_file() || meta.len() > MAX_BYTES {
            bail!("invalid or oversized Git checkpoint file")
        }
        let mut state: Self = serde_json::from_reader(std::fs::File::open(path)?)?;
        if state.schema != 1 || !state.commits.contains_key(&state.head) {
            bail!("unsupported or incomplete Git checkpoint")
        }
        for (oid, recipe) in &state.commits {
            git2::Oid::from_str(oid)?;
            let _: Merkle = recipe.state.parse().context("invalid checkpoint state")?;
            for hash in &recipe.append {
                let _: Hash = hash.parse().context("invalid checkpoint change hash")?;
            }
            if let Some(parent) = &recipe.first_parent
                && !state.commits.contains_key(parent)
            {
                bail!("missing checkpoint parent")
            }
        }
        // Iterative traversal bounds stack usage even for very long histories.
        let mut done = BTreeSet::new();
        for oid in state.commits.keys() {
            let mut visiting = BTreeSet::new();
            let mut next = Some(oid.as_str());
            while let Some(oid) = next {
                if done.contains(oid) {
                    break;
                }
                if !visiting.insert(oid.to_owned()) {
                    bail!("cyclic Git checkpoint")
                }
                next = state.commits[oid].first_parent.as_deref();
            }
            done.extend(visiting);
        }
        state.converted = 0;
        Ok(state)
    }

    fn log(&self, oid: &str) -> Result<Vec<String>, anyhow::Error> {
        let mut recipes = Vec::new();
        let mut next = Some(oid);
        while let Some(oid) = next {
            if recipes.len() > self.commits.len() {
                bail!("cyclic Git checkpoint")
            }
            let recipe = self.commits.get(oid).context("missing Git checkpoint")?;
            recipes.push(recipe);
            next = recipe.first_parent.as_deref();
        }
        Ok(recipes
            .into_iter()
            .rev()
            .flat_map(|r| r.append.iter().cloned())
            .collect())
    }

    pub(super) fn capture<T: TxnTExt>(
        &mut self,
        txn: &T,
        channel: &ChannelRef<T>,
        oid: git2::Oid,
        first_parent: Option<git2::Oid>,
    ) -> Result<(), anyhow::Error> {
        let first_parent = first_parent.map(|p| p.to_string());
        let inherited = match &first_parent {
            Some(parent) => self.log(parent)?,
            None => Vec::new(),
        };
        let log = channel_log(txn, channel)?;
        if !log.starts_with(&inherited) {
            bail!("checkpoint log lost its first-parent prefix")
        }
        let recipe = Recipe {
            first_parent,
            append: log[inherited.len()..].to_vec(),
            state: txn.current_state(&channel.read())?.to_base32(),
        };
        if let Some(previous) = self.commits.get(&oid.to_string())
            && *previous != recipe
        {
            bail!("Git checkpoint changed during reconversion")
        }
        self.commits.insert(oid.to_string(), recipe);
        self.converted += 1;
        Ok(())
    }

    /// Restore an obsolete fork point, or verify the still-live channel.
    pub(super) fn restore<T: MutTxnTExt + TxnTExt + 'static>(
        &self,
        txn: &mut T,
        changes: &pijul_core::changestore::filesystem::FileSystem,
        oid: git2::Oid,
        expected: Merkle,
    ) -> Result<ChannelRef<T>, anyhow::Error> {
        let name = oid.to_string();
        let recipe = self
            .commits
            .get(&name)
            .context("Git db has no checkpoint recipe")?;
        if recipe.state != expected.to_base32() {
            bail!("Git db/checkpoint state mismatch")
        }
        let log = self.log(&name)?;
        let channel = match txn.load_channel(name.parse()?)? {
            Some(channel) => channel,
            None => {
                let name: pijul_core::small_string::SmallString = name.parse()?;
                let channel = txn.open_or_create_channel(&name)?;
                for hash in &log {
                    txn.apply_change(changes, &mut channel.write(), &hash.parse()?)?;
                }
                channel
            }
        };
        if txn.current_state(&channel.read())? != expected || channel_log(txn, &channel)? != log {
            bail!("rebuilt Git checkpoint state or ordered log differs for {oid}")
        }
        Ok(channel)
    }

    /// Never expose a partial export. Called only after all native commits.
    pub(super) fn write(&self, path: &Path) -> Result<(), anyhow::Error> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > MAX_BYTES {
            bail!("Git checkpoint exceeds size limit")
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|error| error.error)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn channel_log<T: TxnTExt>(txn: &T, channel: &ChannelRef<T>) -> Result<Vec<String>, anyhow::Error> {
    txn.log(&channel.read(), 0)?
        .map(|entry| {
            let (_, (hash, _)) = entry?;
            let hash: Hash = hash.into();
            Ok(hash.to_base32())
        })
        .collect()
}
