use crate::alive::{Graph, output_graph};
use crate::changestore::*;
use crate::pristine::*;
use crate::record::Recorded;
use crate::text_encoding::Encoding;
use std::sync::LazyLock;

mod bin;

mod algorithm;
mod split;
mod vertex_buffer;
pub use algorithm::Algorithm;
mod delete;
mod replace;
pub(crate) use replace::is_conflict_marker_line;

pub static DEFAULT_SEPARATOR: LazyLock<regex::bytes::Regex> =
    LazyLock::new(|| regex::bytes::Regex::new("\n").unwrap());

#[derive(Clone, Copy)]
struct Line<'a> {
    l: &'a [u8],
    /// Only set on old lines: lines of a cyclic conflict, and conflict markers
    /// that must not be matched (see [`incoherent_markers`]). Such a line never
    /// equals a new line.
    cyclic: bool,
    before_end_marker: bool,
    last: bool,
    ptr: *const u8,
}

impl<'a> std::fmt::Debug for Line<'a> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "Line {{ l: {:?} }}", std::str::from_utf8(self.l))
    }
}

impl<'a> std::hash::Hash for Line<'a> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.l.hash(state);
    }
}

impl<'a> Default for Line<'a> {
    fn default() -> Self {
        Line {
            l: &[],
            cyclic: false,
            before_end_marker: false,
            last: false,
            ptr: std::ptr::null(),
        }
    }
}

impl<'a> PartialEq for Line<'a> {
    fn eq(&self, b: &Self) -> bool {
        if self.before_end_marker && !b.last && b.l.last() == Some(&b'\n') {
            return &b.l[..b.l.len() - 1] == self.l;
        }
        if b.before_end_marker && !self.last && self.l.last() == Some(&b'\n') {
            return &self.l[..self.l.len() - 1] == b.l;
        }
        ((self.ptr == b.ptr && self.l.len() == b.l.len()) || self.l == b.l)
            && self.cyclic == b.cyclic
    }
}
impl<'a> Eq for Line<'a> {}

#[derive(Error)]
pub enum DiffError<P: std::error::Error + 'static, T: GraphTxnT> {
    #[error(transparent)]
    Output(#[from] crate::output::FileError<P, T>),
    #[error(transparent)]
    Txn(#[from] TxnErr<T::GraphError>),
}

impl<P: std::error::Error, T: GraphTxnT> std::fmt::Debug for DiffError<P, T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DiffError::Output(e) => std::fmt::Debug::fmt(e, fmt),
            DiffError::Txn(e) => std::fmt::Debug::fmt(e, fmt),
        }
    }
}

fn make_old_lines<'a>(d: &'a vertex_buffer::Diff, r: &'a regex::bytes::Regex) -> Vec<Line<'a>> {
    d.lines(r)
        .map(|l| {
            let old_bytes = l.as_ptr() as usize - d.contents_a.as_ptr() as usize;
            let cyclic = if let Err(n) = d
                .cyclic_conflict_bytes
                .binary_search(&(old_bytes, usize::MAX))
            {
                n > 0 && {
                    let (a, b) = d.cyclic_conflict_bytes[n - 1];
                    a <= old_bytes && old_bytes < b
                }
            } else {
                false
            };
            let before_end_marker = if l.last() != Some(&b'\n') {
                let next_index = l.as_ptr() as usize + l.len() - d.contents_a.as_ptr() as usize + 1;
                d.marker.get(&next_index) == Some(&vertex_buffer::ConflictMarker::End)
            } else {
                false
            };
            if log_enabled!(log::Level::Trace) {
                trace!("old = {:?}", l);
            }
            Line {
                l,
                cyclic,
                before_end_marker,
                last: l.as_ptr() as usize + l.len() - d.contents_a.as_ptr() as usize
                    >= d.contents_a.len(),
                ptr: l.as_ptr(),
            }
        })
        .collect()
}

fn make_new_lines<'a>(b: &'a [u8], sep: &'a regex::bytes::Regex) -> Vec<Line<'a>> {
    split::LineSplit::from_bytes_with_sep(b, sep)
        .map(|l| {
            if log_enabled!(log::Level::Trace) {
                trace!("new = {:?}", l);
            }
            let next_index = l.as_ptr() as usize + l.len() - b.as_ptr() as usize;
            Line {
                l,
                cyclic: false,
                before_end_marker: false,
                last: next_index >= b.len(),
                ptr: l.as_ptr(),
            }
        })
        .collect()
}

/// Split the two versions of a file into diffable "lines". This is a pure
/// function of `(d, b, encoding, separator)`, which is what lets the record
/// walk run [`Recorded::diff_prepare`] concurrently and replay the result
/// deterministically in [`Recorded::diff_emit`]: both call this to obtain the
/// exact same `(lines_a, lines_b)`, so the `D` script computed in `prepare`
/// still indexes correctly in `emit`.
fn compute_diff_lines<'a>(
    d: &'a vertex_buffer::Diff,
    b: &'a [u8],
    encoding: &Option<Encoding>,
    separator: &'a regex::bytes::Regex,
    unmatchable: &[usize],
) -> (Vec<Line<'a>>, Vec<Line<'a>>) {
    // TODO pass through both encodings and use that to decide
    if encoding.is_none() {
        const ROLLING_SIZE: usize = 8192;
        debug!("contents_a: {:?}", d.contents_a.len());
        let (ah, old) = bin::make_old_chunks(ROLLING_SIZE, &d.contents_a);
        let (bb, new) = bin::make_new_chunks(ROLLING_SIZE, &ah, b);
        debug!("bb = {:?}", bb);
        (old, new)
    } else {
        let mut old = make_old_lines(d, separator);
        for &i in unmatchable {
            old[i].cyclic = true
        }
        (old, make_new_lines(b, separator))
    }
}

/// The conflict markers of the old lines: `(line, whether it is the first or
/// last marker of its conflict, conflict)`.
fn marker_lines(d: &vertex_buffer::Diff, lines_a: &[Line]) -> Vec<(usize, bool, usize)> {
    if d.marker.is_empty() || lines_a.is_empty() {
        return Vec::new();
    }
    let start = lines_a[0].l.as_ptr() as usize;
    let mut markers: Vec<(usize, bool, usize)> = Vec::new();
    for v in d.pos_a.iter() {
        // Several vertices may share a position (empty vertices, the root
        // before a first marker): the marker is the last root there.
        let Some(marker) = d.marker.get(&v.pos) else {
            continue;
        };
        if !v.vertex.is_root() {
            continue;
        }
        let Ok(line) = lines_a.binary_search_by(|l| (l.l.as_ptr() as usize - start).cmp(&v.pos))
        else {
            continue;
        };
        let is_end_or_begin = *marker != vertex_buffer::ConflictMarker::Next;
        match markers.last_mut() {
            Some(last) if last.0 == line => *last = (line, is_end_or_begin, v.conflict),
            _ => markers.push((line, is_end_or_begin, v.conflict)),
        }
    }
    markers
}

/// Conflict markers matched by `dd` although their conflict is not kept: a
/// conflict stays in the new version only if both its first and last markers
/// are matched. A line of the new version equal to a single marker of a
/// conflict that is otherwise gone is content, not that marker: recording it as
/// the marker would drop it. Adds these lines to `unmatchable`, returns whether
/// there were any.
fn incoherent_markers(
    markers: &[(usize, bool, usize)],
    dd: &algorithm::D,
    unmatchable: &mut Vec<usize>,
) -> bool {
    let is_matched = |line: usize| {
        // `dd` is sorted by `old`.
        let i = dd.r.partition_point(|r| r.old + r.old_len <= line);
        i >= dd.r.len() || dd.r[i].old > line || dd.r[i].old_len == 0
    };
    let mut kept: crate::HashMap<usize, bool> = crate::HashMap::default();
    for &(line, is_end_or_begin, conflict) in markers {
        if is_end_or_begin {
            let k = kept.entry(conflict).or_insert(true);
            *k = *k && is_matched(line)
        }
    }
    let n = unmatchable.len();
    for &(line, _, conflict) in markers {
        if !kept.get(&conflict).unwrap_or(&true) && is_matched(line) && !unmatchable.contains(&line)
        {
            unmatchable.push(line)
        }
    }
    unmatchable.len() > n
}

/// The read-only result of [`Recorded::diff_prepare`]: everything needed to
/// replay a file's diff into the change, without touching the pristine again.
/// Owned and `Send`, so the record walk can compute one per file in parallel
/// and later feed them one at a time to [`Recorded::diff_emit`] in walk order.
pub(crate) struct DiffPlan {
    d: vertex_buffer::Diff,
    dd: algorithm::D,
    b: Vec<u8>,
    encoding: Option<Encoding>,
    /// Old lines that must not be matched, see [`incoherent_markers`].
    unmatchable: Vec<usize>,
}

impl Recorded {
    /// Read-only half of [`Recorded::diff`]: reconstruct the pristine side of
    /// the file (`output_graph`), split both sides into lines and run the diff
    /// algorithm, returning a [`DiffPlan`]. This touches only per-file state
    /// (`self.redundant`, `self.largest_file`) and reads the pristine, so it is
    /// safe to run for many files in parallel. The writes to the shared change
    /// (`actions`, `contents`) all happen later in [`Recorded::diff_emit`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn diff_prepare<T: ChannelTxnT, P: ChangeStore>(
        &mut self,
        changes: &P,
        txn: &ArcTxn<T>,
        channel: &ChannelRef<T>,
        algorithm: Algorithm,
        stop_early: bool,
        path: String,
        inode: Position<Option<ChangeId>>,
        a: &mut Graph,
        b: Vec<u8>,
        encoding: Option<Encoding>,
        separator: &regex::bytes::Regex,
    ) -> Result<DiffPlan, DiffError<P::Error, T>> {
        self.largest_file = self.largest_file.max(b.len() as u64);
        let mut d = vertex_buffer::Diff::new(inode, path, a);
        output_graph(changes, txn, channel, &mut d, a, &mut self.redundant)?;
        let mut unmatchable = Vec::new();
        let mut markers = None;
        let dd = loop {
            let (lines_a, lines_b) = compute_diff_lines(&d, &b, &encoding, separator, &unmatchable);
            trace!("pos = {:?}", d.pos_a);
            if log::log_enabled!(log::Level::Trace) {
                for l in lines_a.iter() {
                    trace!("a: {:?}", l)
                }
                for l in lines_b.iter() {
                    trace!("b: {:?}", l)
                }
            }
            let dd = algorithm::diff(&lines_a, &lines_b, algorithm, stop_early);
            // With `stop_early`, `dd` is incomplete, but only tells whether the
            // file changed, which unmatching markers does not change.
            if stop_early || encoding.is_none() {
                break dd;
            }
            let markers = markers.get_or_insert_with(|| marker_lines(&d, &lines_a));
            // Each round unmatches at least one more marker.
            if !incoherent_markers(markers, &dd, &mut unmatchable) {
                break dd;
            }
            debug!("unmatchable markers: {:?}", unmatchable);
        };
        Ok(DiffPlan {
            d,
            dd,
            b,
            encoding,
            unmatchable,
        })
    }

    /// Write-only half of [`Recorded::diff`]: replay a [`DiffPlan`] (from
    /// [`Recorded::diff_prepare`]) into the change, appending to the shared
    /// `contents`/`actions`. Must run single-threaded and in walk order so the
    /// byte offsets it assigns into `contents` are deterministic.
    pub(crate) fn diff_emit<T: ChannelTxnT, P: ChangeStore>(
        &mut self,
        txn: &ArcTxn<T>,
        channel: &ChannelRef<T>,
        plan: &DiffPlan,
        inode_: Inode,
        separator: &regex::bytes::Regex,
    ) -> Result<(), DiffError<P::Error, T>> {
        let DiffPlan {
            d,
            dd,
            b,
            encoding,
            unmatchable,
        } = plan;
        let txn = txn.read();
        let channel = channel.read();
        let (lines_a, lines_b) = compute_diff_lines(d, b, encoding, separator, unmatchable);
        let mut conflict_contexts = replace::ConflictContexts::new();
        for r in 0..dd.len() {
            if dd[r].old_len > 0 {
                self.delete(
                    &*txn,
                    txn.graph(&*channel),
                    d,
                    dd,
                    &mut conflict_contexts,
                    &lines_a,
                    &lines_b,
                    inode_,
                    r,
                    encoding,
                )?;
            }
            if dd[r].new_len > 0 {
                self.replace(
                    d,
                    &mut conflict_contexts,
                    &lines_a,
                    &lines_b,
                    inode_,
                    dd,
                    r,
                    encoding,
                );
            }
        }
        debug!("Diff ended");
        Ok(())
    }

    /// Combined prepare + emit, kept for tests and callers that diff a single
    /// file synchronously. The record walk uses [`Recorded::diff_prepare`] /
    /// [`Recorded::diff_emit`] directly so it can parallelize the prepare step.
    #[cfg_attr(not(test), allow(dead_code))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn diff<T: ChannelTxnT, P: ChangeStore>(
        &mut self,
        changes: &P,
        txn: &ArcTxn<T>,
        channel: &ChannelRef<T>,
        algorithm: Algorithm,
        stop_early: bool,
        path: String,
        inode_: Inode,
        inode: Position<Option<ChangeId>>,
        a: &mut Graph,
        b: Vec<u8>,
        encoding: Option<Encoding>,
        separator: &regex::bytes::Regex,
    ) -> Result<(), DiffError<P::Error, T>> {
        let plan = self.diff_prepare::<T, P>(
            changes, txn, channel, algorithm, stop_early, path, inode, a, b, encoding, separator,
        )?;
        self.diff_emit::<T, P>(txn, channel, &plan, inode_, separator)
    }
}

// Position of the first byte in chunks[old].
//
// This assumes that all lines in `chunks` come from the same memory region.
fn bytes_pos(chunks: &[Line], old: usize) -> usize {
    if old < chunks.len() {
        chunks[old].l.as_ptr() as usize - chunks[0].l.as_ptr() as usize
    } else if old > 0 {
        chunks[old - 1].l.as_ptr() as usize - chunks[0].l.as_ptr() as usize
            + chunks[old - 1].l.len()
    } else {
        0
    }
}

// Total length (i.e. sum of lengths) in bytes of lines `&chunks[old...old+len]`.
//
// This assumes that all lines in `chunks` come from the same memory region.
fn bytes_len(chunks: &[Line], old: usize, len: usize) -> usize {
    // An empty range is zero bytes. Without this, `old == chunks.len()` (a
    // zero-length range at the very end, which `bytes_pos` already handles)
    // indexes `chunks[old]` out of bounds below.
    if len == 0 {
        return 0;
    }
    if let Some(p) = chunks.get(old + len) {
        p.l.as_ptr() as usize - chunks[old].l.as_ptr() as usize
    } else if old + len > 0 {
        chunks[old + len - 1].l.as_ptr() as usize + chunks[old + len - 1].l.len()
            - chunks[old].l.as_ptr() as usize
    } else {
        chunks[old + len].l.as_ptr() as usize - chunks[old].l.as_ptr() as usize
    }
}
