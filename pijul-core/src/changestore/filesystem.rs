use super::*;
use crate::change::{Change, ChangeFile};
use crate::pristine::{Base32, ChangeId, Hash, Merkle, Vertex};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// When the known cache reaches this many entries, or `KNOWN_CACHE_MAX_BYTES`
/// of hashes, it is cleared wholesale and rebuilt lazily, rather than evicting
/// entries one by one. Most entries are tiny (two small `Vec<Hash>`), but a
/// change with a very long dependency list is not, hence the byte bound; the
/// occasional clear is amortised over many lookups.
const KNOWN_CACHE_MAX: usize = 1 << 16;
const KNOWN_CACHE_MAX_BYTES: usize = 64 << 20;

/// The known cache: entries plus the (approximate) bytes of hashes they hold.
#[derive(Default)]
struct KnownCache {
    map: HashMap<Hash, (Vec<Hash>, Vec<Hash>)>,
    bytes: usize,
}

/// How many times `load` re-opens a file that was replaced while it was being
/// opened, before giving up on caching the handle.
const LOAD_ATTEMPTS: usize = 4;

/// Per-key invalidation counters, striped: a key maps to one of
/// `GENERATION_STRIPES` counters, so a bump only (spuriously, and harmlessly)
/// makes concurrent loads of 1/64th of the keys skip caching.
struct Generations([AtomicU64; GENERATION_STRIPES]);
const GENERATION_STRIPES: usize = 64;

impl Generations {
    fn new() -> Self {
        Generations(std::array::from_fn(|_| AtomicU64::new(0)))
    }
    fn stripe(&self, change: &ChangeId) -> &AtomicU64 {
        let n = (change.0).0;
        &self.0[(n.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize % GENERATION_STRIPES]
    }
    fn get(&self, change: &ChangeId) -> u64 {
        self.stripe(change).load(Ordering::SeqCst)
    }
    fn bump(&self, change: &ChangeId) {
        self.stripe(change).fetch_add(1, Ordering::SeqCst);
    }
}

/// One open change file in the cache. Each entry has its own lock, so
/// reading the contents of one change (a zstd frame decompression) does not
/// block readers of other changes.
pub(crate) type CachedChange = Arc<Mutex<ChangeFile>>;

/// A file system change store.
///
/// Clones share the change cache and the known cache: worker threads of
/// `output` and `record` get a clone each, and a cold cache per clone made
/// them re-open (and re-deserialize) every change file they touched.
pub struct FileSystem {
    change_cache: Arc<Mutex<lru_cache::LruCache<ChangeId, CachedChange>>>,
    /// Cache of the "known" information (dependencies and extra known
    /// changes) of a change, keyed by its hash. This is queried
    /// repeatedly during `apply` (once per deleted parent edge) and is
    /// immutable for a given hash, so entries never need invalidating.
    known_cache: Arc<Mutex<KnownCache>>,
    /// Bumped (under the `change_cache` lock) whenever a change file is
    /// replaced, so that a load that raced with the replacement can tell.
    generations: Arc<Generations>,
    changes_dir: PathBuf,
}

impl Clone for FileSystem {
    fn clone(&self) -> Self {
        FileSystem {
            changes_dir: self.changes_dir.clone(),
            change_cache: self.change_cache.clone(),
            known_cache: self.known_cache.clone(),
            generations: self.generations.clone(),
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
            change_cache: Arc::new(Mutex::new(lru_cache::LruCache::new(cap.max(1)))),
            known_cache: Arc::new(Mutex::new(KnownCache::default())),
            generations: Arc::new(Generations::new()),
        }
    }

    /// Number of open change files the cache keeps (shared by all clones).
    pub fn cache_capacity(&self) -> usize {
        self.change_cache.lock().capacity()
    }

    /// The open change file of `change`, opening it on a cache miss. The
    /// cache lock is not held while the file is opened (that reads and
    /// deserializes its whole hashed section).
    pub(crate) fn load<F: Fn(ChangeId) -> Option<Hash>>(
        &self,
        hash: F,
        change: ChangeId,
    ) -> Result<CachedChange, crate::change::ChangeError> {
        self.load_with(hash, change, || ())
    }

    /// [`Self::load`], calling `between_open_and_insert` once the file has been
    /// opened and before the handle is offered to the cache. Tests use it to
    /// interleave a concurrent replacement at the exact point of the race.
    pub(crate) fn load_with<F: Fn(ChangeId) -> Option<Hash>>(
        &self,
        hash: F,
        change: ChangeId,
        between_open_and_insert: impl FnOnce(),
    ) -> Result<CachedChange, crate::change::ChangeError> {
        let mut between_open_and_insert = Some(between_open_and_insert);
        for attempt in 1..=LOAD_ATTEMPTS {
            if let Some(p) = self.change_cache.lock().get_mut(&change) {
                return Ok(p.clone());
            }
            // Read the generation *before* opening: an invalidation after this
            // point may have replaced the file under our feet.
            let generation = self.generations.get(&change);
            let h = hash(change).unwrap();
            let path = self.filename(&h);
            debug!("changefile: {:?}", path);
            let p = Arc::new(Mutex::new(crate::change::ChangeFile::open(
                h,
                path.to_str().unwrap(),
            )?));
            debug!("patch done");
            if let Some(f) = between_open_and_insert.take() {
                f()
            }
            let mut cache = self.change_cache.lock();
            // Another thread may have opened it meanwhile: keep theirs.
            if let Some(q) = cache.get_mut(&change) {
                return Ok(q.clone());
            }
            // `invalidate` bumps the generation under this same lock, so
            // checking it here is atomic with inserting. If it moved, our
            // handle may be of the file that was just replaced: never cache it.
            if self.generations.get(&change) == generation {
                cache.insert(change, p.clone());
                return Ok(p);
            }
            if attempt == LOAD_ATTEMPTS {
                // Still racing with writers: hand out the handle uncached.
                return Ok(p);
            }
        }
        unreachable!()
    }

    /// Drop the cached handle of `change` (its file was just replaced) and
    /// make loads that started before this call discard what they opened.
    fn invalidate(&self, change: &ChangeId) {
        let mut cache = self.change_cache.lock();
        cache.remove(change);
        self.generations.bump(change);
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
            self.invalidate(change_id);
        }
        Ok(())
    }
}

impl ChangeStore for FileSystem {
    type Error = Error;
    fn has_contents(&self, hash: Hash, change_id: Option<ChangeId>) -> bool {
        if let Some(ref change_id) = change_id
            && let Some(l) = self.change_cache.lock().get_mut(change_id).cloned()
        {
            return l.lock().has_contents();
        }
        let path = self.filename(&hash);
        match crate::change::ChangeFile::open(hash, path.to_str().unwrap()) {
            Ok(p) => p.has_contents(),
            _ => false,
        }
    }

    fn knows(&self, hash0: &Hash, hash1: &Hash) -> Result<bool, Self::Error> {
        let mut cache = self.known_cache.lock();
        if let Some((deps, known)) = cache.map.get(hash0) {
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
        let bytes = (deps.len() + known.len()) * std::mem::size_of::<Hash>();
        if cache.map.len() >= KNOWN_CACHE_MAX || cache.bytes + bytes > KNOWN_CACHE_MAX_BYTES {
            cache.map.clear();
            cache.bytes = 0;
        }
        // An entry that alone exceeds the bound is not worth keeping.
        if bytes <= KNOWN_CACHE_MAX_BYTES {
            cache.bytes += bytes;
            cache.map.insert(*hash0, (deps, known));
        }
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
        let p = self.load(hash, key.change)?;
        let n = p.lock().read_contents(key.start.into(), buf)?;
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
        let p = self.load(hash, change)?;
        let p = p.lock();
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
    fn get_change_without_contents(&self, h: &Hash) -> Result<Change, Self::Error> {
        let file_name = self.filename(h);
        let file_name = file_name.to_str().unwrap();
        debug!("file_name = {:?}", file_name);
        Ok(Change::deserialize_without_contents(file_name, Some(h))?)
    }
}

impl FileSystem {
    /// Replace the unhashed part of the stored change `hash` (e.g. to add an
    /// author signature after the fact), keeping the hashed bytes verbatim so
    /// the hash — and the file name — cannot change. Atomic: written to a
    /// temporary file in the changes directory and renamed over the original.
    pub fn set_unhashed(
        &self,
        hash: &Hash,
        unhashed: Option<&serde_json::Value>,
    ) -> Result<(), Error> {
        let file_name = self.filename(hash);
        let buf = std::fs::read(&file_name)?;
        let out = Change::replace_unhashed_in_buffer(&buf, unhashed)?;
        let mut f = tempfile::NamedTempFile::new_in(&self.changes_dir)?;
        std::io::Write::write_all(&mut f, &out)?;
        f.persist(file_name)?;
        Ok(())
    }
}
