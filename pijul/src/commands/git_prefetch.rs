//! Owned Git inputs for recording, prepared one commit ahead of the importer.
//! No git2 reference crosses a thread boundary. The producer has its own
//! Repository; record workers share a different, mutex-protected object reader.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(super) struct Entry {
    pub oid: git2::Oid,
    pub mode: i32,
    pub is_dir: bool,
}

pub(super) struct Delta {
    pub old: PathBuf,
    pub new: PathBuf,
    pub mode: git2::FileMode,
    pub status: git2::Delta,
    /// Blob sizes (0 for an absent side or a submodule), from the object
    /// headers; `u64::MAX` if the header cannot be read.
    pub old_size: u64,
    pub new_size: u64,
}

pub(super) struct Prepared {
    pub oid: git2::Oid,
    pub files: Arc<HashMap<String, Entry>>,
    // Preserve libgit2's tree walk and parent/delta order, not hash-map order.
    pub tree_order: Vec<(PathBuf, bool)>,
    pub parents: Vec<Vec<Delta>>,
    pub elapsed: Duration,
}

impl Prepared {
    #[cfg(test)]
    fn load_empty(_: &git2::Repository, oid: git2::Oid) -> Result<Self, anyhow::Error> {
        Ok(Self {
            oid,
            files: Arc::new(HashMap::new()),
            tree_order: Vec::new(),
            parents: Vec::new(),
            elapsed: Duration::ZERO,
        })
    }

    pub fn load(git: &git2::Repository, oid: git2::Oid) -> Result<Self, anyhow::Error> {
        let start = Instant::now();
        let commit = git.find_commit(oid)?;
        let tree = commit.tree()?;
        let mut files = HashMap::new();
        let mut tree_order = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |prefix, entry| {
            if let Ok(name) = entry.name() {
                let path = format!("{prefix}{name}");
                let is_dir = entry.kind() == Some(git2::ObjectType::Tree);
                tree_order.push((PathBuf::from(&path), is_dir));
                files.insert(
                    path,
                    Entry {
                        oid: entry.id(),
                        mode: entry.filemode(),
                        is_dir,
                    },
                );
            }
            git2::TreeWalkResult::Ok
        })?;
        let mut parents = Vec::new();
        for parent in commit.parents() {
            let old = parent.tree()?;
            let mut diff = git.diff_tree_to_tree(Some(&old), Some(&tree), None)?;
            diff.find_similar(None)?;
            let odb = git.odb()?;
            let size = |f: git2::DiffFile<'_>| {
                if f.id().is_zero() || f.mode() == git2::FileMode::Commit {
                    0
                } else {
                    odb.read_header(f.id()).map_or(u64::MAX, |h| h.0 as u64)
                }
            };
            parents.push(
                diff.deltas()
                    .map(|d| Delta {
                        old: d.old_file().path().unwrap().to_path_buf(),
                        new: d.new_file().path().unwrap().to_path_buf(),
                        mode: d.new_file().mode(),
                        status: d.status(),
                        old_size: size(d.old_file()),
                        new_size: size(d.new_file()),
                    })
                    .collect(),
            );
        }
        Ok(Self {
            oid,
            files: Arc::new(files),
            tree_order,
            parents,
            elapsed: start.elapsed(),
        })
    }
}

pub(super) struct Prefetch {
    receiver: Option<mpsc::Receiver<Result<Prepared, anyhow::Error>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Prefetch {
    pub fn start(path: &Path, order: Vec<git2::Oid>) -> Self {
        Self::start_with(path, order, Prepared::load)
    }

    fn start_with(
        path: &Path,
        order: Vec<git2::Oid>,
        load: impl Fn(&git2::Repository, git2::Oid) -> Result<Prepared, anyhow::Error> + Send + 'static,
    ) -> Self {
        // At most one queued snapshot plus the one being prepared. No history
        // of blobs/trees is accumulated, including when recording is slower.
        let (sender, receiver) = mpsc::sync_channel(1);
        let path = path.to_path_buf();
        let worker = std::thread::spawn(move || {
            // A panic must reach the importer as an error that says why, not
            // as an anonymous closed channel.
            let panic_sender = sender.clone();
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let git = match git2::Repository::open(path) {
                    Ok(git) => git,
                    Err(e) => {
                        let _ = sender.send(Err(e.into()));
                        return;
                    }
                };
                for oid in order {
                    let prepared = load(&git, oid);
                    let failed = prepared.is_err();
                    if sender.send(prepared).is_err() || failed {
                        break;
                    }
                }
            }));
            if let Err(payload) = run {
                let cause = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string());
                let _ = panic_sender.send(Err(anyhow::anyhow!(
                    "Git prefetch thread panicked: {cause}"
                )));
            }
        });
        Self {
            receiver: Some(receiver),
            worker: Some(worker),
        }
    }

    pub fn next(&self, oid: git2::Oid) -> Result<Prepared, anyhow::Error> {
        let p =
            self.receiver.as_ref().unwrap().recv().map_err(|_| {
                anyhow::anyhow!("Git prefetch thread ended before the import did")
            })??;
        anyhow::ensure!(p.oid == oid, "Git prefetch order differs from import order");
        Ok(p)
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        // Unblock send on every early-return/error path before joining.
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_tree_snapshot_and_early_drop() -> Result<(), anyhow::Error> {
        let dir = tempfile::tempdir()?;
        let git = git2::Repository::init(dir.path())?;
        let blob = git.blob(b"hello\n")?;
        let mut builder = git.treebuilder(None)?;
        builder.insert("file", blob, 0o100755)?;
        let tree = git.find_tree(builder.write()?)?;
        let author = git2::Signature::now("test", "test@example.invalid")?;
        let oid = git.commit(Some("HEAD"), &author, &author, "root", &tree, &[])?;
        let p = Prefetch::start(git.path(), vec![oid; 4]);
        let snapshot = p.next(oid)?;
        assert_eq!(snapshot.files["file"].oid, blob);
        assert_eq!(snapshot.files["file"].mode, 0o100755);
        assert!(!snapshot.files["file"].is_dir);
        assert_eq!(snapshot.tree_order, vec![(PathBuf::from("file"), false)]);
        assert!(snapshot.parents.is_empty());
        // There are still results to send. Joining must not wait forever for
        // a bounded queue that nobody will drain after an importer error.
        drop(p);
        Ok(())
    }

    #[test]
    fn worker_open_failure_reaches_importer() -> Result<(), anyhow::Error> {
        let dir = tempfile::tempdir()?;
        let oid = git2::Oid::ZERO_SHA1;
        let p = Prefetch::start(dir.path(), vec![oid]);
        assert!(p.next(oid).is_err());
        Ok(())
    }

    #[test]
    fn worker_panic_cause_reaches_importer() -> Result<(), anyhow::Error> {
        let dir = tempfile::tempdir()?;
        let git = git2::Repository::init(dir.path())?;
        let a = git2::Oid::from_bytes(&[1; 20])?;
        let b = git2::Oid::from_bytes(&[2; 20])?;
        let p = Prefetch::start_with(git.path(), vec![a, b], move |git, oid| {
            if oid == b {
                panic!("boom while preparing {oid}");
            }
            Prepared::load_empty(git, oid)
        });
        assert_eq!(p.next(a)?.oid, a);
        let err = p.next(b).err().expect("the panic must be an error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("panicked") && msg.contains("boom while preparing"),
            "{msg}"
        );
        Ok(())
    }
}
