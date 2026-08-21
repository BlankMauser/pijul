use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use crate::commands::common_opts::RepoPath;
use crate::commands::load_channel_exact;
use anyhow::bail;
use byteorder::{BigEndian, WriteBytesExt};
use clap::Parser;
use log::{debug, error, warn};
use pijul_core::*;
use regex::Regex;

/// This command is not meant to be run by the user,
/// instead it is called over SSH
#[derive(Parser, Debug)]
pub struct Protocol {
    #[clap(flatten)]
    base: RepoPath,
    /// Use this protocol version
    #[clap(long = "version")]
    version: usize,
}

static APPLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"apply\s+(\S+)\s+([^ ]*) ([0-9]+)\s+"#).unwrap());
static ARCHIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"archive\s+(\S+)\s*(( ([^:]+))*)( :(.*))?\n"#).unwrap());
static CHANGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"((change)|(partial))\s+([^ ]*)\s+"#).unwrap());
static CHANGELIST_PATHS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(((\\")|[^"])+)""#).unwrap());
static CHANGELIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"changelist\s+(\S+)\s+([0-9]+)(.*)\s+"#).unwrap());
// static CHANNEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"channel\s+(\S+)\s+"#).unwrap());
static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"id\s+(\S+)\s+"#).unwrap());
static IDENTITIES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"identities(\s+([0-9]+))?\s+"#).unwrap());
static STATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"state\s+(\S+)(\s+([0-9]+)?)\s+"#).unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^tag\s+(\S+)\s+"#).unwrap());
static TAGUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^tagup\s+(\S+)\s+(\S+)\s+([0-9]+)\s+"#).unwrap());

const PARTIAL_CHANGE_SIZE: u64 = 1 << 20;

impl Protocol {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self) -> Result<(), anyhow::Error> {
        let mut repo = self.base.find_root()?;
        let pristine = Arc::new(repo.pristine);
        let txn = pristine.arc_txn_begin()?;
        let mut ws = pijul_core::ApplyWorkspace::new();
        let mut buf = String::new();
        let mut buf2 = vec![0; 4096 * 10];
        let s = std::io::stdin();
        let mut s = s.lock();
        let o = std::io::stdout();
        let mut o = BufWriter::new(o.lock());
        let mut applied = HashMap::new();

        debug!("reading");
        while s.read_line(&mut buf)? > 0 {
            debug!("{:?}", buf);
            if let Some(cap) = ID.captures(&buf) {
                let channel = load_channel_exact(&cap[1], &*txn.read())?;
                let c = channel.read();
                writeln!(o, "{}", c.id)?;
                o.flush()?;
            } else if let Some(cap) = STATE.captures(&buf) {
                let channel = load_channel_exact(&cap[1], &*txn.read())?;
                let init = if let Some(u) = cap.get(3) {
                    u.as_str().parse().ok()
                } else {
                    None
                };
                if let Some(pos) = init {
                    let txn = txn.read();
                    for x in txn.log(&*channel.read(), pos)? {
                        let (n, (_, m)) = x?;
                        match n.cmp(&pos) {
                            std::cmp::Ordering::Less => continue,
                            std::cmp::Ordering::Greater => {
                                writeln!(o, "-")?;
                                break;
                            }
                            std::cmp::Ordering::Equal => {
                                let m: pijul_core::Merkle = m.into();
                                let m2 = if let Some(x) = txn
                                    .rev_iter_tags(txn.tags(&*channel.read()), Some(n))?
                                    .next()
                                {
                                    x?.1.b.into()
                                } else {
                                    Merkle::zero()
                                };
                                writeln!(o, "{} {} {}", n, m.to_base32(), m2.to_base32())?;
                                break;
                            }
                        }
                    }
                } else {
                    let txn = txn.read();
                    if let Some(x) = txn.reverse_log(&*channel.read(), None)?.next() {
                        let (n, (_, m)) = x?;
                        let m: Merkle = m.into();
                        let m2 = if let Some(x) = txn
                            .rev_iter_tags(txn.tags(&*channel.read()), Some(n))?
                            .next()
                        {
                            x?.1.b.into()
                        } else {
                            Merkle::zero()
                        };
                        writeln!(o, "{} {} {}", n, m.to_base32(), m2.to_base32())?
                    } else {
                        writeln!(o, "-")?;
                    }
                }
                o.flush()?;
            } else if let Some(cap) = CHANGELIST.captures(&buf) {
                let channel = load_channel_exact(&cap[1], &*txn.read())?;
                let from: u64 = cap[2].parse().unwrap();
                let mut paths = Vec::new();
                let txn = txn.read();
                {
                    for r in CHANGELIST_PATHS.captures_iter(&cap[3]) {
                        let s: String = r[1].replace("\\\"", "\"");
                        if let Ok((p, ambiguous)) =
                            txn.follow_oldest_path(&repo.changes, &channel, &s)
                        {
                            if ambiguous {
                                bail!("Ambiguous path")
                            }
                            let h: pijul_core::Hash =
                                txn.get_external(&p.change).optional()?.unwrap().into();
                            writeln!(o, "{}.{}", h.to_base32(), p.pos.0)?;
                            paths.push(s);
                        } else {
                            debug!("protocol line: {:?}", buf);
                            bail!("Protocol error")
                        }
                    }
                }
                let mut tagsi = 0;
                (pijul_remote::local::Local {
                    channel: (&cap[1]).to_string(),
                    root: PathBuf::new(),
                    changes_dir: PathBuf::new(),
                    pristine: pristine.clone(),
                    name: String::new(),
                })
                .download_changelist_(
                    |_, n, h, m, is_tag| {
                        if is_tag {
                            writeln!(o, "{}.{}.{}.", n, h.to_base32(), m.to_base32())?;
                            tagsi += 1;
                        } else {
                            writeln!(o, "{}.{}.{}", n, h.to_base32(), m.to_base32())?;
                        }
                        Ok(())
                    },
                    &mut (),
                    from,
                    &paths,
                    &*txn,
                    &channel,
                )?;
                writeln!(o)?;
                o.flush()?;
            } else if let Some(_cap) = TAG.captures(&buf) {
                bail!("Tag file format has been removed")
            } else if let Some(_cap) = TAGUP.captures(&buf) {
                bail!("Tag file format has been removed")
            } else if let Some(cap) = CHANGE.captures(&buf) {
                let h_ = &cap[4];
                let h = if let Some(h) = Hash::from_base32(h_.as_bytes()) {
                    h
                } else {
                    debug!("protocol error: {:?}", buf);
                    bail!("Protocol error")
                };
                pijul_core::changestore::filesystem::push_filename(&mut repo.changes_dir, &h);
                debug!("repo = {:?}", repo.changes_dir);
                let mut f = std::fs::File::open(&repo.changes_dir)?;
                let size = std::fs::metadata(&repo.changes_dir)?.len();
                let size = if &cap[1] == "change" || size <= PARTIAL_CHANGE_SIZE {
                    size
                } else {
                    pijul_core::change::Change::size_no_contents(&mut f)?
                };
                o.write_u64::<BigEndian>(size)?;
                let mut size = size as usize;
                while size > 0 {
                    if size < buf2.len() {
                        buf2.truncate(size as usize);
                    }
                    let n = f.read(&mut buf2[..])?;
                    if n == 0 {
                        break;
                    }
                    size -= n;
                    o.write_all(&buf2[..n])?;
                }
                o.flush()?;
                pijul_core::changestore::filesystem::pop_filename(&mut repo.changes_dir);
            } else if let Some(cap) = APPLY.captures(&buf) {
                let h = if let Some(h) = Hash::from_base32(cap[2].as_bytes()) {
                    h
                } else {
                    debug!("protocol error {:?}", buf);
                    bail!("Protocol error");
                };
                let mut path = repo.changes_dir.clone();
                pijul_core::changestore::filesystem::push_filename(&mut path, &h);
                std::fs::create_dir_all(path.parent().unwrap())?;
                let size: usize = cap[3].parse().unwrap();
                buf2.resize(size, 0);
                s.read_exact(&mut buf2)?;
                std::fs::write(&path, &buf2)?;
                pijul_core::change::Change::deserialize(&path.to_string_lossy(), Some(&h))?;
                let channel = load_channel_exact(&cap[1], &*txn.read())?;
                {
                    let mut channel_ = channel.write();
                    txn.write()
                        .apply_change_ws(&repo.changes, &mut channel_, &h, &mut ws)?;
                }
                applied
                    .entry(cap[1].to_string())
                    .or_insert_with(|| (channel, Vec::new()))
                    .1
                    .push(h);
            } else if let Some(cap) = ARCHIVE.captures(&buf) {
                let mut w = Vec::new();
                let mut tarball = pijul_core::output::Tarball::new(
                    &mut w,
                    cap.get(6).map(|x| x.as_str().to_string()),
                    0,
                );
                let channel = load_channel_exact(&cap[1], &*txn.read())?;
                let conflicts = if let Some(caps) = cap.get(2) {
                    debug!("caps = {:?}", caps.as_str());
                    let mut hashes = caps.as_str().split(' ').filter(|x| !x.is_empty());
                    let state: pijul_core::Merkle = hashes.next().unwrap().parse().unwrap();
                    let extra: Vec<pijul_core::Hash> = hashes.map(|x| x.parse().unwrap()).collect();
                    debug!("state = {:?}, extra = {:?}", state, extra);
                    if txn.read().current_state(&*channel.read())? == state && extra.is_empty() {
                        txn.archive(&repo.changes, &channel, &mut tarball)?
                    } else {
                        use rand::RngExt;
                        let fork_name: pijul_core::small_string::SmallString = rand::rng()
                            .sample_iter(&rand::distr::Alphanumeric)
                            .take(30)
                            .map(|x| x as char)
                            .collect::<String>()
                            .parse()?;
                        let mut fork = {
                            let mut txn = txn.write();
                            txn.fork(&channel, &fork_name)?
                        };
                        let conflicts = txn.archive_with_state(
                            &repo.changes,
                            &mut fork,
                            &state,
                            &extra,
                            &mut tarball,
                            0,
                        )?;
                        txn.write().drop_channel(&fork_name)?;
                        conflicts
                    }
                } else {
                    txn.archive(&repo.changes, &channel, &mut tarball)?
                };
                std::mem::drop(tarball);
                let mut o = std::io::stdout();
                o.write_u64::<BigEndian>(w.len() as u64)?;
                o.write_u64::<BigEndian>(conflicts.len() as u64)?;
                o.write_all(&w)?;
                o.flush()?;
            } else if let Some(cap) = IDENTITIES.captures(&buf) {
                let last_touched: u64 = if let Some(last) = cap.get(2) {
                    last.as_str().parse().unwrap()
                } else {
                    0
                };
                let mut id_dir = repo.path.clone();
                id_dir.push(DOT_DIR);
                id_dir.push("identities");
                let r = if let Ok(r) = std::fs::read_dir(&id_dir) {
                    r
                } else {
                    writeln!(o)?;
                    o.flush()?;
                    continue;
                };
                let mut at_least_one = false;
                for id in r {
                    at_least_one |= output_id(id, last_touched, &mut o).unwrap_or(false);
                }
                debug!("at least one {:?}", at_least_one);
                if !at_least_one {
                    writeln!(o)?;
                }
                writeln!(o)?;
                o.flush()?;
            } else {
                error!("unmatched")
            }
            buf.clear();
        }
        let applied_nonempty = !applied.is_empty();
        for (_, (channel, hashes)) in applied {
            // Output only the files touched by the applied changes. Rewriting
            // the whole working copy (prefix "", `if_modified_since` None) bumps
            // every file's mtime, which invalidates `record`'s stat cache and
            // makes the next `pijul record` re-diff the entire tree. Mirrors the
            // touched-files logic of `pijul pull` (see pushpull.rs).
            let mut touched = HashSet::new();
            {
                let txn_ = txn.read();
                for h in hashes.iter() {
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
                    if let Some(pijul_core::fs::FindPath { path, .. }) =
                        pijul_core::fs::find_path(&repo.changes, &*txn_, &*channel_, false, i)?
                    {
                        touched_paths.insert(path.join("/"));
                    } else {
                        // Path unresolved: fall back to a full re-output.
                        touched_paths.clear();
                        touched_paths.insert(String::new());
                        break;
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
                    &repo.working_copy,
                    &repo.changes,
                    &txn,
                    &channel,
                    &path,
                    true,
                    None,
                    std::thread::available_parallelism()?.get(),
                    0,
                )?;
                last = Some(path);
            }
        }
        if applied_nonempty {
            txn.commit()?;
        }
        Ok(())
    }
}

fn output_id<W: Write>(
    id: Result<std::fs::DirEntry, std::io::Error>,
    last_touched: u64,
    mut o: W,
) -> Result<bool, anyhow::Error> {
    let id = id?;
    let m = id.metadata()?;
    let p = id.path();
    debug!("{:?}", p);
    let mod_ts = m
        .modified()?
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if mod_ts >= last_touched {
        let mut done = HashSet::new();
        if p.file_name() == Some("publickey.json".as_ref()) {
            warn!("Skipping serializing old public key format.");
            return Ok(false);
        } else {
            let mut idf = if let Ok(f) = std::fs::File::open(&p) {
                f
            } else {
                return Ok(false);
            };
            let id: Result<pijul_identity::Complete, _> = serde_json::from_reader(&mut idf);
            if let Ok(id) = id {
                if !done.insert(id.public_key.key.clone()) {
                    return Ok(false);
                }
                serde_json::to_writer(&mut o, &id.as_portable()).unwrap();
                writeln!(o)?;
                return Ok(true);
            }
        }
    }
    Ok(false)
}
