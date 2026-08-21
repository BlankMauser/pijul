use std::collections::HashSet;
use std::path::{Path, PathBuf};

use canonical_path::CanonicalPathBuf;
use clap::{Parser, ValueHint};
use log::debug;
use pijul_core::changestore::ChangeStore;
use pijul_core::vertex_buffer::{
    END_MARKER, SEPARATOR, START_MARKER, VertexBuffer, change_message,
};
use pijul_core::*;
use serde_derive::Serialize;

use crate::commands::common_opts::{OutputFormat, RepoAndChannel};
use crate::commands::load_channel;

#[derive(Parser, Debug)]
pub struct Credit {
    #[clap(flatten)]
    base: RepoAndChannel,
    /// Output the annotations as structured JSON instead of the default
    /// human-readable, per-line change attribution.
    #[clap(long = "output-format", value_enum)]
    output_format: Option<OutputFormat>,
    /// The file to annotate
    #[clap(value_hint = ValueHint::FilePath)]
    file: PathBuf,
}

impl Credit {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let has_repo_path = self.base.repo_path().is_some();
        let repo = self.base.find_root()?;
        let txn_ = repo.pristine.arc_txn_begin()?;
        let txn = txn_.read();
        let (channel, _) = load_channel(self.base.channel(), &*txn)?;
        let repo_path = CanonicalPathBuf::canonicalize(&repo.path)?;
        let (pos, _ambiguous) = if has_repo_path {
            let root = std::fs::canonicalize(repo.path.join(&self.file))?;
            let path = root.strip_prefix(&repo_path.as_path())?.to_str().unwrap();
            txn.follow_oldest_path(&repo.changes, &channel, &path)?
        } else {
            let mut root = std::env::current_dir()?;
            root.push(&self.file);
            let root = std::fs::canonicalize(&root)?;
            let path = root.strip_prefix(&repo_path.as_path())?.to_str().unwrap();
            txn.follow_oldest_path(&repo.changes, &channel, &path)?
        };
        std::mem::drop(txn);

        // Suppress the "broken pipe" errors that occur when the output is
        // piped into a program that closes early (e.g. `head`).
        macro_rules! ignore_broken_pipe {
            ($e:expr) => {
                match $e {
                    Ok(_) => {}
                    Err(pijul_core::output::FileError::Io(io))
                        if io.kind() == std::io::ErrorKind::BrokenPipe => {}
                    Err(e) => return Err(e.into()),
                }
            };
        }

        match self.output_format.unwrap_or_default() {
            OutputFormat::Json => {
                let mut creditor = JsonCreditor::new(txn_.clone(), channel.clone());
                ignore_broken_pipe!(pijul_core::output::output_file(
                    &repo.changes,
                    &txn_,
                    &channel,
                    pos,
                    &mut creditor,
                ));
                serde_json::to_writer_pretty(std::io::stdout(), &creditor.entries)?;
                println!();
            }
            OutputFormat::Plaintext => {
                super::pager(config);
                ignore_broken_pipe!(pijul_core::output::output_file(
                    &repo.changes,
                    &txn_,
                    &channel,
                    pos,
                    &mut Creditor::new(std::io::stdout(), txn_.clone(), channel.clone()),
                ));
            }
        }
        Ok(())
    }
}

pub struct Creditor<W: std::io::Write, T: ChannelTxnT> {
    w: W,
    buf: Vec<u8>,
    new_line: bool,
    changes: HashSet<Hash>,
    txn: ArcTxn<T>,
    channel: ChannelRef<T>,
}

impl<W: std::io::Write, T: ChannelTxnT> Creditor<W, T> {
    pub fn new(w: W, txn: ArcTxn<T>, channel: ChannelRef<T>) -> Self {
        Creditor {
            w,
            new_line: true,
            buf: Vec::new(),
            txn,
            channel,
            changes: HashSet::new(),
        }
    }
}

impl<W: std::io::Write, T: TxnTExt> VertexBuffer for Creditor<W, T> {
    fn output_line<E, C: FnOnce(&mut [u8]) -> Result<(), E>>(
        &mut self,
        v: Vertex<ChangeId>,
        c: C,
    ) -> Result<(), E>
    where
        E: From<std::io::Error>,
    {
        debug!("outputting vertex {:?}", v);
        self.buf.resize(v.end - v.start, 0);
        c(&mut self.buf)?;

        if !v.change.is_root() {
            self.changes.clear();
            let txn = self.txn.read();
            let channel = self.channel.read();
            for e in txn
                .iter_adjacent(&channel, v, EdgeFlags::PARENT, EdgeFlags::all())
                .unwrap()
            {
                let e = e.unwrap();
                if e.introduced_by().is_root() {
                    continue;
                }
                if let Ok(Some(intro)) = txn.get_external(&e.introduced_by()).optional() {
                    self.changes.insert(intro.into());
                }
            }
            if !self.new_line {
                writeln!(self.w)?;
            }
            writeln!(self.w)?;
            let mut is_first = true;
            for c in self.changes.drain() {
                let c = c.to_base32();
                write!(
                    self.w,
                    "{}{}",
                    if is_first { "" } else { ", " },
                    c.split_at(12).0,
                )?;
                is_first = false;
            }
            writeln!(self.w, "\n")?;
        }
        let ends_with_newline = self.buf.ends_with(b"\n");
        if let Ok(s) = std::str::from_utf8(&self.buf[..]) {
            for l in s.lines() {
                self.w.write_all(b"> ")?;
                self.w.write_all(l.as_bytes())?;
                self.w.write_all(b"\n")?;
            }
        }
        if !self.buf.is_empty() {
            // empty "lines" (such as in the beginning of a file)
            // don't change the status of self.new_line.
            self.new_line = ends_with_newline;
        }
        Ok(())
    }

    fn output_conflict_marker<C: ChangeStore>(
        &mut self,
        marker: &str,
        id: usize,
        sides: Option<(&C, &[&Hash])>,
    ) -> Result<(), std::io::Error> {
        if !self.new_line {
            self.w.write_all(b"\n")?;
        }
        write!(self.w, "{} {}", marker, id)?;
        match sides {
            Some((changes, sides)) => {
                for side in sides.into_iter() {
                    let h = side.to_base32();
                    write!(
                        self.w,
                        " [{} {}]",
                        h.split_at(8).0,
                        change_message(changes, side)
                    )?;
                }
            }
            None => (),
        };
        self.w.write_all(b"\n")?;
        Ok(())
    }
}

/// One item in the structured `credit --output-format json` stream.
/// Replaying the entries in order reconstructs the annotated file, with
/// each run of lines attributed to the change(s) that introduced it and
/// conflicts delimited by explicit markers.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum CreditEntry {
    /// A run of one or more lines sharing the same attribution.
    Line {
        /// 1-based line number where this run begins.
        #[serde(rename = "startLine")]
        start_line: usize,
        /// Number of lines this run covers.
        #[serde(rename = "lineCount")]
        line_count: usize,
        /// Full base32 hashes of the changes that introduced these lines.
        changes: Vec<String>,
        /// The conflict id these lines belong to, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        conflict: Option<usize>,
        content: String,
    },
    /// A conflict boundary (start / separator / end).
    Conflict {
        /// One of "start", "separator", "end".
        marker: &'static str,
        id: usize,
        /// Full base32 hashes of the sides involved, when known.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        sides: Vec<String>,
    },
}

/// A [`VertexBuffer`] that accumulates structured, machine-readable credit
/// annotations instead of writing human-readable text. All the per-line
/// change attribution is already computed by the graph walk; this simply
/// keeps the line geometry (which the text renderer discards) and the full
/// change hashes (which the text renderer truncates).
pub struct JsonCreditor<T: ChannelTxnT> {
    buf: Vec<u8>,
    /// Number of completed lines emitted so far (i.e. newlines seen).
    line: usize,
    new_line: bool,
    /// Stack of currently-open conflict ids, innermost last.
    conflicts: Vec<usize>,
    txn: ArcTxn<T>,
    channel: ChannelRef<T>,
    entries: Vec<CreditEntry>,
}

impl<T: ChannelTxnT> JsonCreditor<T> {
    pub fn new(txn: ArcTxn<T>, channel: ChannelRef<T>) -> Self {
        JsonCreditor {
            buf: Vec::new(),
            line: 0,
            new_line: true,
            conflicts: Vec::new(),
            txn,
            channel,
            entries: Vec::new(),
        }
    }
}

impl<T: TxnTExt> VertexBuffer for JsonCreditor<T> {
    fn output_line<E, C: FnOnce(&mut [u8]) -> Result<(), E>>(
        &mut self,
        v: Vertex<ChangeId>,
        c: C,
    ) -> Result<(), E>
    where
        E: From<std::io::Error>,
    {
        self.buf.resize(v.end - v.start, 0);
        c(&mut self.buf)?;

        // The line this vertex starts on (1-based), whether or not the
        // previous vertex ended on a line boundary.
        let start_line = self.line + 1;
        let ends_with_newline = self.buf.ends_with(b"\n");
        self.line += self.buf.iter().filter(|c| **c == b'\n').count();

        if self.buf.is_empty() {
            // Empty "lines" (e.g. at the start of a file) don't advance
            // the line cursor and carry no attribution.
            return Ok(());
        }

        // Same attribution the text `Creditor` computes: the set of
        // (non-root) changes that introduced the edges pointing at this
        // vertex. We keep the *full* hash rather than the 12-char prefix.
        let mut changes = Vec::new();
        if !v.change.is_root() {
            let mut seen = HashSet::new();
            let txn = self.txn.read();
            let channel = self.channel.read();
            for e in txn
                .iter_adjacent(&channel, v, EdgeFlags::PARENT, EdgeFlags::all())
                .unwrap()
            {
                let e = e.unwrap();
                if e.introduced_by().is_root() {
                    continue;
                }
                if let Ok(Some(intro)) = txn.get_external(&e.introduced_by()).optional() {
                    let h: Hash = intro.into();
                    if seen.insert(h) {
                        changes.push(h.to_base32());
                    }
                }
            }
        }

        // Stable ordering so the output is deterministic across runs.
        changes.sort();

        let content = String::from_utf8_lossy(&self.buf).into_owned();
        let line_count = content.lines().count().max(1);
        self.entries.push(CreditEntry::Line {
            start_line,
            line_count,
            changes,
            conflict: self.conflicts.last().copied(),
            content,
        });

        self.new_line = ends_with_newline;
        Ok(())
    }

    fn output_conflict_marker<C: ChangeStore>(
        &mut self,
        marker: &str,
        id: usize,
        sides: Option<(&C, &[&Hash])>,
    ) -> Result<(), std::io::Error> {
        // Mirror the text buffers' line accounting so line numbers stay
        // aligned with a rendered view that includes the markers.
        if self.new_line {
            self.line += 1;
        } else {
            self.line += 2;
        }
        self.new_line = true;

        let (kind, opens, closes) = match marker {
            START_MARKER => ("start", true, false),
            SEPARATOR => ("separator", false, false),
            END_MARKER => ("end", false, true),
            _ => ("start", true, false),
        };
        if opens {
            self.conflicts.push(id);
        }
        let sides = sides
            .map(|(_, hs)| hs.iter().map(|h| h.to_base32()).collect())
            .unwrap_or_default();
        self.entries.push(CreditEntry::Conflict {
            marker: kind,
            id,
            sides,
        });
        if closes {
            if let Some(pos) = self.conflicts.iter().rposition(|c| *c == id) {
                self.conflicts.remove(pos);
            }
        }
        Ok(())
    }
}
