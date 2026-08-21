use super::*;
use crate::change::{Change, ChangeFile};
use crate::pristine::{Base32, ChangeId, Hash, Merkle, Vertex};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// When the known cache reaches this many entries it is cleared
/// wholesale and rebuilt lazily, rather than evicting entries one by
/// one. Each entry is tiny (two small `Vec<Hash>`), so this bound is
/// generous, and the occasional clear is amortised over many lookups.
const KNOWN_CACHE_MAX: usize = 1 << 16;

/// A file system change store.
pub struct FileSystem {
    change_cache: Mutex<lru_cache::LruCache<ChangeId, ChangeFile>>,
    /// Cache of the "known" information (dependencies and extra known
    /// changes) of a change, keyed by its hash. This is queried
    /// repeatedly during `apply` (once per deleted parent edge) and is
    /// immutable for a given hash, so entries never need invalidating.
    known_cache: Mutex<HashMap<Hash, (Vec<Hash>, Vec<Hash>)>>,
    changes_dir: PathBuf,
}

impl Clone for FileSystem {
    fn clone(&self) -> Self {
        let len = self.change_cache.lock().capacity();
        FileSystem {
            changes_dir: self.changes_dir.clone(),
            change_cache: Mutex::new(lru_cache::LruCache::new(len)),
            known_cache: Mutex::new(HashMap::new()),
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    #[error(transparent)]
    ChangeFile(#[from] crate::change::ChangeError),
    #[error(transparent)]
    Persist(#[from] tempfile::PersistError),
}

pub fn push_filename(changes_dir: &mut PathBuf, hash: &Hash) {
    let h32 = hash.to_base32();
    let (a, b) = h32.split_at(2);
    changes_dir.push(a);
    changes_dir.push(b);
    changes_dir.set_extension("change");
}

pub fn push_tag_filename(changes_dir: &mut PathBuf, hash: &Merkle) {
    let h32 = hash.to_base32();
    let (a, b) = h32.split_at(2);
    changes_dir.push(a);
    changes_dir.push(b);
    changes_dir.set_extension("tag");
}

pub fn pop_filename(changes_dir: &mut PathBuf) {
    changes_dir.pop();
    changes_dir.pop();
}

impl FileSystem {
    pub fn filename(&self, hash: &Hash) -> PathBuf {
        let mut path = self.changes_dir.clone();
        push_filename(&mut path, hash);
        path
    }

    pub fn tag_filename(&self, hash: &Merkle) -> PathBuf {
        let mut path = self.changes_dir.clone();
        push_tag_filename(&mut path, hash);
        path
    }

    pub fn has_change(&self, hash: &Hash) -> bool {
        std::fs::metadata(self.filename(hash)).is_ok()
    }

    /// Construct a `FileSystem`, starting from the root of the
    /// repository (i.e. the parent of the `.pijul` directory).
    pub fn from_root<P: AsRef<Path>>(root: P, cap: usize) -> Self {
        let dot_pijul = root.as_ref().join(crate::DOT_DIR);
        let changes_dir = dot_pijul.join("changes");
        Self::from_changes(changes_dir, cap)
    }

    /// Construct a `FileSystem`, starting from the root of the
    /// repository (i.e. the parent of the `.pijul` directory).
    pub fn from_changes(changes_dir: PathBuf, cap: usize) -> Self {
        std::fs::create_dir_all(&changes_dir).unwrap();
        FileSystem {
            changes_dir,
            change_cache: Mutex::new(lru_cache::LruCache::new(cap)),
            known_cache: Mutex::new(HashMap::new()),
        }
    }

    fn load<'a, F: Fn(ChangeId) -> Option<Hash>>(
        &'a self,
        hash: F,
        change: ChangeId,
    ) -> Result<
        parking_lot::MutexGuard<'a, lru_cache::LruCache<ChangeId, ChangeFile>>,
        crate::change::ChangeError,
    > {
        let mut change_cache = self.change_cache.lock();
        if !change_cache.contains_key(&change) {
            let h = hash(change).unwrap();
            let path = self.filename(&h);
            debug!("changefile: {:?}", path);
            let p = crate::change::ChangeFile::open(h, path.to_str().unwrap())?;
            debug!("patch done");
            change_cache.insert(change, p);
        }
        Ok(change_cache)
    }

    pub fn save_from_buf(
        &self,
        buf: &[u8],
        hash: &Hash,
        change_id: Option<ChangeId>,
    ) -> Result<(), crate::change::ChangeError> {
        Change::check_from_buffer(buf, hash)?;
        self.save_from_buf_unchecked(buf, hash, change_id)?;
        Ok(())
    }

    pub fn save_from_buf_unchecked(
        &self,
        buf: &[u8],
        hash: &Hash,
        change_id: Option<ChangeId>,
    ) -> Result<(), std::io::Error> {
        let mut f = tempfile::NamedTempFile::new_in(&self.changes_dir)?;
        let file_name = self.filename(hash);
        use std::io::Write;
        f.write_all(buf)?;
        debug!("file_name = {:?}", file_name);
        std::fs::create_dir_all(file_name.parent().unwrap())?;
        f.persist(file_name)?;
        if let Some(ref change_id) = change_id {
            self.change_cache.lock().remove(change_id);
        }
        Ok(())
    }
}

impl ChangeStore for FileSystem {
    type Error = Error;
    fn has_contents(&self, hash: Hash, change_id: Option<ChangeId>) -> bool {
        if let Some(ref change_id) = change_id
            && let Some(l) = self.change_cache.lock().get_mut(change_id)
        {
            return l.has_contents();
        }
        let path = self.filename(&hash);
        match crate::change::ChangeFile::open(hash, path.to_str().unwrap()) {
            Ok(p) => p.has_contents(),
            _ => false,
        }
    }

    fn knows(&self, hash0: &Hash, hash1: &Hash) -> Result<bool, Self::Error> {
        let mut cache = self.known_cache.lock();
        if let Some((deps, known)) = cache.get(hash0) {
            return Ok(known.contains(hash1) || deps.contains(hash1));
        }
        // Not cached: open the change and remember only its "known"
        // information, which is all `knows` needs and is immutable for
        // a given hash. This avoids re-deserializing the whole change
        // (all hunks) on every call, as the default trait impl does.
        let path = self.filename(hash0);
        let p = crate::change::ChangeFile::open(*hash0, path.to_str().unwrap())?;
        let hashed = p.hashed();
        let deps = hashed.dependencies.clone();
        let known = hashed.extra_known.clone();
        let result = known.contains(hash1) || deps.contains(hash1);
        if cache.len() >= KNOWN_CACHE_MAX {
            cache.clear();
        }
        cache.insert(*hash0, (deps, known));
        Ok(result)
    }

    fn get_header(&self, h: &Hash) -> Result<ChangeHeader, Self::Error> {
        let path = self.filename(h);
        let p = crate::change::ChangeFile::open(*h, path.to_str().unwrap())?;
        Ok(p.hashed().header.clone())
    }

    fn get_contents<F: Fn(ChangeId) -> Option<Hash>>(
        &self,
        hash: F,
        key: Vertex<ChangeId>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error> {
        debug!("get_contents {:?}", key);
        if key.end <= key.start || key.is_root() {
            debug!("return 0");
            return Ok(0);
        }
        assert_eq!(key.end - key.start, buf.len());
        let mut cache = self.load(hash, key.change)?;
        let p = cache.get_mut(&key.change).unwrap();
        let n = p.read_contents(key.start.into(), buf)?;
        debug!("get_contents {:?}", n);
        Ok(n)
    }
    fn get_contents_ext(
        &self,
        key: Vertex<Option<Hash>>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error> {
        if let Some(change) = key.change {
            assert_eq!(key.end.us() - key.start.us(), buf.len());
            if key.end <= key.start {
                return Ok(0);
            }
            let path = self.filename(&change);
            let mut p = crate::change::ChangeFile::open(change, path.to_str().unwrap())?;
            let n = p.read_contents(key.start.into(), buf)?;
            Ok(n)
        } else {
            Ok(0)
        }
    }
    fn change_deletes_position<F: Fn(ChangeId) -> Option<Hash>>(
        &self,
        hash: F,
        change: ChangeId,
        pos: Position<Option<Hash>>,
    ) -> Result<Vec<Hash>, Self::Error> {
        let mut cache = self.load(hash, change)?;
        let p = cache.get_mut(&change).unwrap();
        let mut v = Vec::new();
        for c in p.hashed().changes.iter() {
            for c in c.iter() {
                v.extend(c.deletes_pos(pos))
            }
        }
        Ok(v)
    }
    fn save_change<
        E: From<Self::Error> + From<ChangeError>,
        F: FnOnce(&mut Change, &Hash) -> Result<(), E>,
    >(
        &self,
        p: &mut Change,
        ff: F,
    ) -> Result<Hash, E> {
        let mut f = match tempfile::NamedTempFile::new_in(&self.changes_dir) {
            Ok(f) => f,
            Err(e) => return Err(E::from(Error::from(e))),
        };
        let hash = {
            let w = std::io::BufWriter::new(&mut f);
            p.serialize(w, ff)?
        };
        let file_name = self.filename(&hash);
        if let Err(e) = std::fs::create_dir_all(file_name.parent().unwrap()) {
            return Err(E::from(Error::from(e)));
        }
        debug!("file_name = {:?}", file_name);
        if let Err(e) = f.persist(file_name) {
            return Err(E::from(Error::from(e)));
        }
        Ok(hash)
    }
    fn del_change(&self, hash: &Hash) -> Result<bool, Self::Error> {
        let file_name = self.filename(hash);
        debug!("file_name = {:?}", file_name);
        let result = std::fs::remove_file(&file_name).is_ok();
        std::fs::remove_dir(file_name.parent().unwrap()).unwrap_or(()); // fails silently if there are still changes with the same 2-letter prefix.
        Ok(result)
    }
    fn get_change(&self, h: &Hash) -> Result<Change, Self::Error> {
        let file_name = self.filename(h);
        let file_name = file_name.to_str().unwrap();
        debug!("file_name = {:?}", file_name);
        Ok(Change::deserialize(file_name, Some(h))?)
    }
}
