use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::convert::TryFrom;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::bail;
use clap::Parser;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use log::*;
use serde::ser::{SerializeSeq, Serializer};
use thiserror::*;

use crate::commands::common_opts::{OutputFormat, RepoAndChannel};
use crate::commands::load_channel;
use pijul_core::changestore::*;
use pijul_core::pristine::{
    ChannelRef, DepsTxnT, GraphTxnT, Hash, TreeErr, TreeTxnT, TxnErr, sanakirja::Txn,
};
use pijul_core::{Base32, TxnTExt};
use pijul_repository::Repository;

/// A struct containing user-input assembled by Parser.
#[derive(Parser, Debug)]
pub struct Log {
    #[clap(flatten)]
    base: RepoAndChannel,
    /// Only show the change hashes
    #[clap(long = "hash-only", conflicts_with = "files")]
    hash_only: bool,
    /// Include state identifiers in the output
    #[clap(long = "state")]
    states: bool,
    /// Include full change description in the output
    #[clap(long = "description")]
    descriptions: bool,
    /// Include files changed in the output
    #[clap(long = "files")]
    files: bool,
    /// Start after this many changes
    #[clap(long = "offset")]
    offset: Option<usize>,
    /// Output at most this many changes
    #[clap(long = "limit")]
    limit: Option<usize>,
    #[clap(long = "output-format", value_enum)]
    output_format: Option<OutputFormat>,
    /// Filter log output, showing only log entries that touched the specified
    /// files. Accepted as a list of paths relative to your current directory.
    /// Currently, filters can only be applied when logging the channel that's
    /// in use.
    #[clap(last = true)]
    filters: Vec<String>,
}

impl TryFrom<Log> for LogIterator {
    type Error = anyhow::Error;
    fn try_from(mut cmd: Log) -> Result<LogIterator, Self::Error> {
        let repo = Repository::find_root(cmd.repository_path())?;
        let txn = repo.pristine.txn_begin()?;

        let (channel_ref, is_current_channel) = load_channel(cmd.base.channel(), &txn)?;
        // The only situation that's disallowed is if the user's trying to apply
        // path filters AND get the logs for a channel other than the one they're
        // currently using (where using means the one that comprises the working copy)
        if !cmd.filters.is_empty() && !is_current_channel {
            bail!("Currently, log filters can only be applied to the channel currently in use.")
        }

        let limit = cmd.limit.unwrap_or(usize::MAX);
        let offset = cmd.offset.unwrap_or(0);

        let mut id_path = repo.path.join(pijul_core::DOT_DIR);
        id_path.push("identities");
        let show_paths = cmd.files;

        Ok(Self {
            txn,
            repo,
            cmd,
            id_path,
            channel_ref,
            limit,
            offset,
            show_paths,
        })
    }
}

#[derive(Debug, Error)]
pub enum Error<E: std::error::Error> {
    #[error("pijul log couldn't find a file or directory corresponding to `{}`", 0)]
    NotFound(String),
    #[error(transparent)]
    Txn(#[from] pijul_core::pristine::sanakirja::SanakirjaError),
    #[error(transparent)]
    TxnErr(#[from] TxnErr<pijul_core::pristine::sanakirja::SanakirjaError>),
    #[error(transparent)]
    TreeErr(#[from] TreeErr<pijul_core::pristine::sanakirja::SanakirjaError>),
    #[error(transparent)]
    Fs(#[from] pijul_core::FsError<pijul_core::pristine::sanakirja::Txn>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("pijul log couldn't assemble file prefix for pattern `{}`: {} was not a file in the repository at {}", pat, canon_path.display(), repo_path.display())]
    FilterPath {
        pat: String,
        canon_path: PathBuf,
        repo_path: PathBuf,
    },
    #[error(
        "pijul log couldn't assemble file prefix for pattern `{}`: the path contained invalid UTF-8",
        0
    )]
    InvalidUtf8(String),
    #[error(transparent)]
    E(E),
    #[error(transparent)]
    Filesystem(#[from] pijul_core::changestore::filesystem::Error),
}

// A lot of error-handling noise here, but since we're dealing with
// a user-command and a bunch of file-IO/path manipulation it's
// probably necessary for the feedback to be good.
fn get_inodes<E: std::error::Error>(
    txn: &Txn,
    repo_path: &Path,
    pats: &[String],
) -> Result<
    Vec<(
        pijul_core::Inode,
        Option<pijul_core::pristine::Position<pijul_core::ChangeId>>,
    )>,
    Error<E>,
> {
    let mut inodes = Vec::new();
    for pat in pats {
        let canon_path = match Path::new(pat).canonicalize() {
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound) => {
                return Err(Error::NotFound(pat.to_string()));
            }
            Err(e) => return Err(e.into()),
            Ok(p) => p,
        };

        match canon_path.strip_prefix(repo_path).map(|p| p.to_str()) {
            // strip_prefix error is if repo_path is not a prefix of canon_path,
            // which would only happen if they pased in a filter path that's not
            // in the repository.
            Err(_) => {
                return Err(Error::FilterPath {
                    pat: pat.to_string(),
                    canon_path,
                    repo_path: repo_path.to_path_buf(),
                });
            }
            // PathBuf.to_str() returns none iff the path contains invalid UTF-8.
            Ok(None) => return Err(Error::InvalidUtf8(pat.to_string())),
            Ok(Some(s)) => {
                let inode = pijul_core::fs::find_inode(txn, s)?;
                let inode_position = txn.get_inodes(&inode, None)?;
                inodes.push((inode, inode_position.cloned()))
            }
        };
    }
    log::debug!("log filters: {:#?}\n", pats);
    Ok(inodes)
}

/// A single log entry created by [`LogIterator`]. The fields are
/// all `Option<T>` so that users can more precisely choose what
/// data they want.
///
/// The implementaiton of [`std::fmt::Display`] is the standard method
/// of pretty-printing a `LogEntry` to the terminal.
#[derive(serde_derive::Serialize)]
#[serde(untagged)]
enum LogEntry {
    Full {
        #[serde(skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        authors: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<Timestamp>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        paths: Option<Vec<String>>,
    },
    Hash(pijul_core::Hash),
    #[cfg(feature = "git")]
    Git {
        commit: String,
        message: String,
    },
}

/// The standard pretty-print
impl std::fmt::Display for LogEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            LogEntry::Full {
                hash,
                state,
                authors,
                timestamp,
                message,
                description,
                paths,
            } => {
                if let Some(h) = hash {
                    writeln!(f, "Change {}", h)?;
                }
                if let Some(authors) = authors {
                    write!(f, "Author: ")?;
                    let mut is_first = true;
                    for a in authors.iter() {
                        if is_first {
                            is_first = false;
                            write!(f, "{}", a)?;
                        } else {
                            write!(f, ", {}", a)?;
                        }
                    }
                    // Write a linebreak after finishing the list of authors.
                    writeln!(f)?;
                }

                if let Some(timestamp) = timestamp {
                    let local_timestamp = timestamp.to_zoned(TimeZone::system());
                    writeln!(
                        f,
                        "Date: {}",
                        local_timestamp.strftime("%Y-%m-%d %H:%M:%S (%Z)")
                    )?;
                }
                if let Some(mrk) = state {
                    writeln!(f, "State: {}", mrk)?;
                }
                if let Some(message) = message {
                    writeln!(f, "\n    {}\n", message)?;
                }
                if let Some(description) = description {
                    writeln!(f, "\n    {}\n", description)?;
                }
                if let Some(paths) = paths {
                    writeln!(f, "   Files:")?;
                    for path in paths {
                        writeln!(f, "       - {}", path)?;
                    }
                    writeln!(f)?;
                }
            }
            LogEntry::Hash(h) => {
                writeln!(f, "{}", h.to_base32())?;
            }
            #[cfg(feature = "git")]
            LogEntry::Git { commit, message } => {
                writeln!(f, "{commit}\n{message}")?;
            }
        }
        Ok(())
    }
}

/// Contains state needed to produce the sequence of [`LogEntry`] items
/// that are to be logged. The implementation of `TryFrom<Log>` provides
/// a fallible way of creating one of these from the CLI's [`Log`] structure.
///
/// The two main things this provides are an efficient/streaming implementation
/// of [`serde::Serialize`], and an implementation of [`std::fmt::Display`] that
/// does the standard pretty-printing to stdout.
///
/// The [`LogIterator::for_each`] method lets us reuse the most code while providing both
/// pretty-printing and efficient serialization; we can't easily do this with
/// a full implementation of Iterator because serde's serialize method requires
/// self to be an immutable reference.
struct LogIterator {
    /// The parsed CLI command
    cmd: Log,
    txn: Txn,
    repo: Repository,
    id_path: PathBuf,
    channel_ref: ChannelRef<Txn>,
    limit: usize,
    offset: usize,
    show_paths: bool,
}

/// This implementation of Serialize is hand-rolled in order
/// to allow for greater re-use and efficiency.
impl serde::Serialize for LogIterator {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(None)?;
        match self.for_each(|entry| seq.serialize_element(&entry)) {
            Ok(_) => seq.end(),
            Err(anyhow_err) => Err(serde::ser::Error::custom(anyhow_err)),
        }
    }
}

/// Pretty-prints all of the requested log entries in the standard
/// user-facing format.
impl std::fmt::Display for LogIterator {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.for_each(|entry| write!(f, "{}", entry)) {
            Err(e) => {
                log::error!("LogIterator::Display: {}", e);
                Err(std::fmt::Error)
            }
            _ => Ok(()),
        }
    }
}

impl LogIterator {
    /// Call `f` on each [`LogEntry`] in a [`LogIterator`].
    ///
    /// The purpose of this is to let us execute a function over the log entries
    /// without having to duplicate the iteration/filtering logic or
    /// having to collect all of the elements first.
    fn for_each<A, E: std::error::Error>(
        &self,
        mut f: impl FnMut(LogEntry) -> Result<A, E>,
    ) -> Result<(), Error<E>> {
        // A cache of authors to keys. Prevents us from having to do
        // a lot of file-io for looking up the same author multiple times.
        let mut authors = HashMap::new();

        let mut id_path = self.id_path.clone();

        let inodes = get_inodes(&self.txn, &self.repo.path, &self.cmd.filters)?;
        let mut offset = self.offset;
        let mut limit = self.limit;

        let mut reverse_log = self
            .txn
            .reverse_log(&*self.channel_ref.read(), None)?
            .peekable();

        if reverse_log.peek().is_none() {
            let mut stderr = std::io::stderr();
            writeln!(stderr, "No matching logs found")?;

            return Ok(());
        }

        for pr in reverse_log {
            let (_, (h, mrk)) = pr?;
            let cid = self.txn.get_internal(h)?.unwrap();
            let mut is_in_filters = inodes.is_empty();
            for (_, position) in inodes.iter() {
                if let Some(position) = position {
                    is_in_filters = self.txn.get_touched_files(position, Some(cid))? == Some(cid);
                    if is_in_filters {
                        break;
                    }
                }
            }
            if is_in_filters {
                if offset == 0 && limit > 0 {
                    // If there were no path filters applied, OR is this was one of the hashes
                    // marked by the file filters that were applied
                    let entry =
                        self.mk_log_entry(&mut authors, &mut id_path, h.into(), Some(mrk.into()))?;
                    f(entry).map_err(Error::E)?;
                    limit -= 1
                } else if limit > 0 {
                    offset -= 1
                } else {
                    break;
                }
            }
        }

        Ok(())
    }

    /// Create a [`LogEntry`] for a given hash.
    ///
    /// Most of this is just getting the right key information from either the cache
    /// or from the relevant file.
    fn mk_log_entry<'x, E: std::error::Error>(
        &self,
        author_kvs: &'x mut HashMap<String, String>,
        id_path: &mut PathBuf,
        h: pijul_core::Hash,
        m: Option<pijul_core::Merkle>,
    ) -> Result<LogEntry, Error<E>> {
        if self.cmd.hash_only {
            return Ok(LogEntry::Hash(h));
        }

        let paths = if self.show_paths {
            let files = self.repo.changes.get_changes(&h)?;
            let mut paths: Vec<String> = files
                .into_iter()
                .map(|hunk| hunk.path().to_string())
                .collect();

            paths.dedup();
            Some(paths)
        } else {
            None
        };

        match h {
            Hash::Blake3(_) => {}
            Hash::None => {}
            #[cfg(feature = "git")]
            Hash::GitSha1(g) => {
                let oid = git2::Oid::from_bytes(&g).unwrap();
                return Ok(LogEntry::Git {
                    commit: format!("Commit {}\n", oid),
                    message: String::new(),
                });
            }
            #[cfg(feature = "git")]
            Hash::GitSha2(g) => {
                let oid = git2::Oid::from_bytes(&g).unwrap();
                return Ok(LogEntry::Git {
                    commit: format!("Commit {}\n", oid),
                    message: String::new(),
                });
            }
            #[allow(unreachable_patterns)]
            #[cfg(not(feature = "git"))]
            _ => {
                bail!("Unsupported hash type, maybe Pijul wasn't compiled with the right features")
            }
        }

        let header = self.repo.changes.get_header(&h.into())?;
        let authors = header
            .authors
            .into_iter()
            .map(|mut auth| {
                if let Some(k) = auth.0.remove("key") {
                    let auth = match author_kvs.entry(k) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(e) => {
                            let mut id = None;
                            id_path.push(e.key());
                            if let Ok(f) = std::fs::File::open(&id_path) {
                                if let Ok(id_) =
                                    serde_json::from_reader::<_, pijul_identity::Complete>(f)
                                {
                                    id = Some(id_)
                                }
                            }
                            id_path.pop();
                            debug!("{:?}", id);

                            if let Ok(identities) = pijul_identity::Complete::load_all() {
                                for identity in identities {
                                    if &identity.public_key.key == e.key() {
                                        id = Some(identity);
                                    }
                                }
                            }

                            if let Some(id) = id {
                                if id.config.author.display_name.is_empty() {
                                    e.insert(id.config.author.username)
                                } else {
                                    if id.config.author.email.is_empty() {
                                        e.insert(format!(
                                            "{} ({})",
                                            id.config.author.display_name,
                                            id.config.author.username
                                        ))
                                    } else {
                                        e.insert(format!(
                                            "{} ({}) <{}>",
                                            id.config.author.display_name,
                                            id.config.author.username,
                                            id.config.author.email
                                        ))
                                    }
                                }
                            } else {
                                let k = e.key().to_string();
                                e.insert(k)
                            }
                        }
                    };
                    auth.to_owned()
                } else {
                    format!(
                        "{}{}",
                        auth.0.get("name").unwrap(),
                        match auth.0.get("email") {
                            Some(email) => format!(" <{}>", email),
                            None => "".to_string(),
                        },
                    )
                }
            })
            .collect();
        Ok(LogEntry::Full {
            hash: Some(h.to_base32()),
            state: m.map(|mm| mm.to_base32()).filter(|_| self.cmd.states),
            authors: Some(authors),
            timestamp: Some(header.timestamp),
            message: Some(header.message.clone()),
            description: if self.cmd.descriptions {
                header.description
            } else {
                None
            },
            paths,
        })
    }
}

impl Log {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    // In order to accommodate both pretty-printing and efficient
    // serialization to a serde target format, this now delegates
    // mostly to [`LogIterator`].
    pub fn run(self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let log_iter = LogIterator::try_from(self)?;
        let mut stdout = std::io::stdout();

        super::pager(config);

        match log_iter.cmd.output_format.unwrap_or_default() {
            OutputFormat::Json => serde_json::to_writer_pretty(&mut stdout, &log_iter)?,
            OutputFormat::Plaintext => {
                log_iter.for_each(|entry| match write!(&mut stdout, "{}", entry) {
                    Ok(_) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                    Err(e) => Err(e),
                })?
            }
        }
        Ok(())
    }
}
