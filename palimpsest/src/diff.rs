//! The annotation engine.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use pijul_core::alive::{output_graph, retrieve, AliveVertex, Graph, Redundant, VertexId};
use pijul_core::change::Atom;
use pijul_core::changestore::ChangeStore;
use pijul_core::pristine::{
    internal_pos, internal_vertex, iter_adj_all, iter_adjacent, sanakirja::Pristine, Base32,
    ChangeId, ChangePosition, ChannelTxnT, EdgeFlags, GraphTxnT, Position, SerializedEdge, Vertex,
    L64,
};
use pijul_core::vertex_buffer::VertexBuffer;
use pijul_core::{Hash, MutTxnT, MutTxnTExt, TxnTExt};
use tracing::*;

// ── Public output shape ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Segment {
    pub kind: SegmentKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentKind {
    /// Context: text untouched by this change.
    Keep,
    /// Text added by this change, still alive.
    Add,
    /// Text deleted by this change (shown as ghost content).
    Del,
    /// Text added by this change, later deleted by another one.
    Obs,
    /// A collapsed run of unchanged text whose content was deliberately
    /// not fetched (it lives in patches we chose not to open). Carries
    /// no `text`; renderers show it as a "hidden lines" marker.
    Skip,
}

/// One vertex (contiguous graph node) of a file, in output order.
///
/// The vertex identity (`change`/`start`/`end`) lets the client refer
/// back to this exact node to lazily fetch its [`text`](Self::text)
/// later (see [`vertex_contents`]) without re-walking the graph.
#[derive(Debug, Serialize)]
pub struct DiffVertex {
    /// Base32 hash of the change that introduced this vertex; empty for
    /// the graph root (which never carries content).
    pub change: String,
    /// Byte offsets of this vertex within `change`.
    pub start: u64,
    pub end: u64,
    pub kind: SegmentKind,
    /// The vertex's decoded text, or `None` when it was not fetched
    /// (the file exceeded the eager threshold and this vertex is
    /// outside the initial window — fetch it on demand) or is not valid
    /// UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The annotated view of one file touched by a change: the whole file's
/// vertices in output order, each with its annotation and (eagerly or
/// lazily) its text.
#[derive(Debug, Serialize)]
pub struct FileDiff {
    /// The file's path in the channel, resolved from the graph
    /// (youngest name). Falls back to the file's graph position
    /// (`HASH:pos`) if the file has no live name.
    pub path: String,
    pub vertices: Vec<DiffVertex>,
    /// Nested structural fold regions (tree-sitter), in display-line
    /// coordinates. Present only in `full` mode for a recognised
    /// language; `None` otherwise (the client then folds by
    /// indentation). See [`crate::folds`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folds: Option<Vec<crate::folds::FoldNode>>,
}

/// The result of [`change_diff`]: every file the change touched, sent
/// whole so the client can fold/expand untouched regions itself.
#[derive(Debug, Serialize)]
pub struct ContextResponse {
    /// `true` when every vertex carries its `text` (the whole change fit
    /// under [`THRESHOLD`] bytes, so the client has a complete local
    /// copy and folds client-side). `false` when only the windowed
    /// vertices carry text and the rest must be lazy-fetched via
    /// [`vertex_contents`].
    pub full: bool,
    pub files: Vec<FileDiff>,
}

/// Total decoded size (summed over every file) below which
/// [`change_diff`] sends every vertex's text eagerly, priming a
/// client-side cache. Above it, only the windowed vertices are sent.
pub const THRESHOLD: usize = 1_000_000;

fn small(s: &str) -> pijul_core::small_string::SmallString {
    pijul_core::small_string::SmallString::from_str(s)
}

/// Unchanged graph vertices kept on each side of a change; longer runs
/// of untouched vertices are collapsed into a [`SegmentKind::Skip`] and
/// their content is never fetched.
const CONTEXT: usize = 3;

/// Full-context, colour-annotated diff of a single committed change
/// against the current state of `channel`, one [`FileDiff`] per file
/// the change touched.
///
/// Needs only the pristine and the change store: file identity and
/// paths come from the channel graph, so this works on bare
/// repositories (and with any [`ChangeStore`] implementation). If the
/// change is not yet on the channel, it is applied (with its
/// dependencies) first.
pub fn change_diff<C: ChangeStore>(
    pristine: &Pristine,
    changes: &C,
    channel: &str,
    hash: Hash,
) -> anyhow::Result<ContextResponse>
where
    C::Error: Send + Sync + 'static,
{
    change_diff_(pristine, changes, channel, hash, THRESHOLD)
}

/// [`change_diff`] with an explicit eager-text threshold (see
/// [`THRESHOLD`]); factored out so tests can force the lazy path on a
/// small fixture.
pub fn change_diff_<C: ChangeStore>(
    pristine: &Pristine,
    changes: &C,
    channel: &str,
    hash: Hash,
    threshold: usize,
) -> anyhow::Result<ContextResponse>
where
    C::Error: Send + Sync + 'static,
{
    debug!("change_diff {:?}", channel);
    let txn_ = pristine.arc_txn_begin()?;
    let channel = {
        let mut t = txn_.write();
        t.open_or_create_channel(&small(channel))?
    };

    // Make sure the change is present in the channel graph, and get its id.
    let instant = std::time::Instant::now();
    let id: ChangeId = {
        let mut txn = txn_.write();
        if let Some(&id) = txn.get_internal(&hash.into())? {
            debug!("apply: {:?}", id);
            if txn.get_revchanges(&channel, &hash)?.is_none() {
                debug!("apply: {:?}", hash);
                let mut chw = channel.write();
                txn.apply_change_rec(changes, &mut chw, &hash)?;
            }
            id
        } else {
            debug!("apply: {:?}", hash);
            let mut chw = channel.write();
            txn.apply_change_rec(changes, &mut chw, &hash)?;
            *txn.get_internal(&hash.into())?.unwrap()
        }
    };
    debug!("apply: {:?}", instant.elapsed());

    // Only the change's atoms (hunks) are needed to annotate the graph;
    // the added bytes are read later from the graph via `get_contents`.
    // `get_changes` lets a change store hand back just the hashed section
    // (no contents decompression), so we never fully deserialize a
    // change here — like outputting a file, which reads bytes directly.
    let hunks = changes
        .get_changes(&hash)
        .map_err(|e| anyhow::anyhow!("get_changes: {e}"))?;

    let mut annotations: HashMap<Vertex<ChangeId>, Ann> = HashMap::new();
    // One alive graph per file (inode position) the change touched.
    let mut graphs: HashMap<Position<ChangeId>, G> = HashMap::new();
    // The path each touched file had when this change was recorded.
    let mut hunk_paths: HashMap<Position<ChangeId>, String> = HashMap::new();

    {
        let txnr = txn_.read();
        let txn = &*txnr;
        let instant = std::time::Instant::now();

        for h in hunks.iter() {
            for atom in h.iter() {
                let is_folder = match atom {
                    Atom::NewVertex(n) => n.flag.contains(EdgeFlags::FOLDER),
                    Atom::EdgeMap(n) => {
                        n.edges.is_empty() || n.edges[0].flag.contains(EdgeFlags::FOLDER)
                    }
                };
                if is_folder {
                    continue;
                }

                let inode = internal_pos(txn, &atom.inode(), id)?;
                hunk_paths
                    .entry(inode)
                    .or_insert_with(|| h.path().to_string());

                let chan = channel.read();
                let graph = txn.graph(&chan);
                let g = match graphs.entry(inode) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let gg = retrieve(txn, graph, inode, false)?;
                        let al: HashMap<Vertex<ChangeId>, usize> =
                            gg.lines.iter().map(|x| x.vertex).zip(0..).collect();
                        e.insert(G { g: gg, al })
                    }
                };

                match atom {
                    Atom::NewVertex(n) => {
                        let mut pos = n.start;
                        while pos < n.end {
                            if let Ok(b) = txn.find_block(graph, Position { change: id, pos }) {
                                let b = *b;
                                let is_alive = iter_adjacent(
                                    txn,
                                    graph,
                                    b,
                                    EdgeFlags::PARENT | EdgeFlags::BLOCK,
                                    EdgeFlags::PARENT | EdgeFlags::BLOCK,
                                )?
                                .next()
                                .is_some();
                                if is_alive {
                                    annotations.insert(b, Ann::Add);
                                } else {
                                    reinsert_deleted(txn, &chan, &mut annotations, None, b, id, g)?;
                                }
                                pos = b.end;
                            } else {
                                break;
                            }
                        }
                    }
                    Atom::EdgeMap(e) => {
                        for edge in &e.edges {
                            if edge.flag.contains(EdgeFlags::DELETED) {
                                let from = internal_pos(txn, &edge.from, id)?;
                                let to = internal_vertex(txn, &edge.to, id)?;
                                reinsert_deleted(
                                    txn,
                                    &chan,
                                    &mut annotations,
                                    Some(from),
                                    to,
                                    id,
                                    g,
                                )?;
                            } else {
                                let mut pos = edge.to.start;
                                let change = if let Some(h) = edge.to.change {
                                    *txn.get_internal(&h.into())?.unwrap()
                                } else {
                                    id
                                };
                                let mut blk = *txn.find_block(graph, Position { change, pos })?;
                                annotations.insert(blk, Ann::Add);
                                while blk.end < edge.to.end {
                                    pos = blk.end;
                                    blk = *txn.find_block(graph, Position { change: id, pos })?;
                                    annotations.insert(blk, Ann::Add);
                                }
                            }
                        }
                    }
                }
            }
        }
        debug!("atoms {:?}", instant.elapsed());
    }

    // First pass: walk each file's graph (the expensive step) to get
    // the output order and annotation of every vertex. `VBuf` records
    // each vertex without invoking the content-fetch closure, so
    // `output_graph` opens no patch files here — content is fetched in
    // the second pass, for the shown vertices only.
    struct FileRows {
        path: String,
        rows: Vec<(Vertex<ChangeId>, RowKind)>,
    }
    let mut file_rows: Vec<FileRows> = Vec::new();
    for (inode_pos, mut g) in graphs {
        let mut buf = VBuf {
            rows: Vec::new(),
            annotations: &annotations,
        };
        let mut forward: Vec<Redundant> = Vec::new();
        let instant = std::time::Instant::now();
        output_graph(changes, &txn_, &channel, &mut buf, &mut g.g, &mut forward)
            .map_err(|e| anyhow::anyhow!("output_graph: {e}"))?;
        debug!("output_graph: {:?}", instant.elapsed());

        // Resolve the file's (youngest) path from the graph itself, so
        // no working copy is needed. A file with no live name (deleted
        // since) is named by the path its hunk recorded: `find_path` would
        // only yield its live ancestors. Failing both, fall back to its
        // graph position.
        let path = {
            let txn = txn_.read();
            let chan = channel.read();
            let named = iter_adjacent(
                &*txn,
                txn.graph(&chan),
                inode_pos.inode_vertex(),
                EdgeFlags::FOLDER | EdgeFlags::PARENT,
                EdgeFlags::all(),
            )?
            .any(|e| {
                e.is_ok_and(|e| {
                    e.flag().contains(EdgeFlags::FOLDER | EdgeFlags::PARENT)
                        && !e.flag().contains(EdgeFlags::DELETED)
                })
            });
            let recorded = hunk_paths
                .get(&inode_pos)
                .filter(|p| !named && !p.is_empty());
            match (
                recorded,
                pijul_core::fs::find_path(changes, &*txn, &chan, true, inode_pos),
            ) {
                (Some(recorded), _) => recorded.clone(),
                (None, Ok(Some(fp))) if !fp.path.is_empty() => fp.path.join("/"),
                _ => {
                    let h: Hash = txn
                        .get_external(&inode_pos.change)
                        .map(|h| h.into())
                        .unwrap_or(Hash::None);
                    format!("{}:{}", h.to_base32(), u64::from(inode_pos.pos.0))
                }
            }
        };
        file_rows.push(FileRows {
            path,
            rows: buf.rows,
        });
    }

    // If the whole change fits under `threshold` bytes, send every
    // vertex's text so the client can fold/expand entirely on its own;
    // otherwise send text only for the windowed vertices (changed rows
    // plus CONTEXT around each) and let the client lazy-fetch the rest.
    let total: usize = file_rows
        .iter()
        .flat_map(|f| f.rows.iter())
        .map(|(v, _)| v.end - v.start)
        .sum();
    let full = total <= threshold;

    let mut files = Vec::with_capacity(file_rows.len());
    let mut content: Vec<u8> = Vec::new();
    for FileRows { path, rows } in file_rows {
        let n = rows.len();
        // Which vertices carry their text in this response.
        let shown = if full {
            vec![true; n]
        } else {
            let mut shown = vec![false; n];
            for i in 0..n {
                if rows[i].1 != RowKind::Keep {
                    let lo = i.saturating_sub(CONTEXT);
                    let hi = (i + CONTEXT).min(n.saturating_sub(1));
                    for s in shown.iter_mut().take(hi + 1).skip(lo) {
                        *s = true;
                    }
                }
            }
            shown
        };

        let instant = std::time::Instant::now();
        let mut vertices = Vec::with_capacity(n);
        for (i, (vertex, kind)) in rows.into_iter().enumerate() {
            let text = if shown[i] {
                content.resize(vertex.end - vertex.start, 0);
                changes
                    .get_contents(
                        |p| txn_.read().get_external(&p).ok().map(|x| x.into()),
                        vertex,
                        &mut content,
                    )
                    .map_err(|e| anyhow::anyhow!("get_contents: {e}"))?;
                std::str::from_utf8(&content).ok().map(|s| s.to_string())
            } else {
                None
            };
            let change: Hash = txn_
                .read()
                .get_external(&vertex.change)
                .map(|h| h.into())
                .unwrap_or(Hash::None);
            let kind = match kind {
                RowKind::Keep => SegmentKind::Keep,
                RowKind::Add => SegmentKind::Add,
                RowKind::Del => SegmentKind::Del,
                RowKind::Obs => SegmentKind::Obs,
            };
            vertices.push(DiffVertex {
                change: if change == Hash::None {
                    String::new()
                } else {
                    change.to_base32()
                },
                start: vertex.start.into(),
                end: vertex.end.into(),
                kind,
                text,
            });
        }
        debug!("contents: {:?}", instant.elapsed());
        // Structural folds only make sense with the whole file present.
        let folds = if full {
            crate::folds::language_for(&path).map(|lang| {
                let (alive_text, map) = alive_text_and_map(&vertices);
                crate::folds::compute_folds(&lang, &alive_text, &map)
            })
        } else {
            None
        };
        files.push(FileDiff {
            path,
            vertices,
            folds,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ContextResponse { full, files })
}

/// Fetch the decoded text of specific vertices without walking the
/// graph — the cheap counterpart to [`change_diff`], used to lazily
/// fill in vertices whose `text` was omitted (see
/// [`DiffVertex::text`]). Results are parallel to `refs`; an entry is
/// `None` when the change is unknown to this pristine or the bytes are
/// not valid UTF-8.
///
/// A vertex's bytes live in the change store and its change is already
/// in the pristine's internal table (it is a change of this
/// repository), so this needs only a read transaction — no channel, no
/// apply, no `output_graph`.
pub fn vertex_contents<C: ChangeStore>(
    pristine: &Pristine,
    changes: &C,
    refs: &[(Hash, u64, u64)],
) -> anyhow::Result<Vec<Option<String>>>
where
    C::Error: Send + Sync + 'static,
{
    let txn = pristine.txn_begin()?;
    let mut out = Vec::with_capacity(refs.len());
    let mut content: Vec<u8> = Vec::new();
    for (change, start, end) in refs {
        let id = match txn.get_internal(&(*change).into())? {
            Some(id) => *id,
            None => {
                out.push(None);
                continue;
            }
        };
        let vertex = Vertex {
            change: id,
            start: ChangePosition(L64::from(*start)),
            end: ChangePosition(L64::from(*end)),
        };
        content.resize((end - start) as usize, 0);
        let text = match changes.get_contents(
            |p| txn.get_external(&p).ok().map(|x| x.into()),
            vertex,
            &mut content,
        ) {
            Ok(_) => std::str::from_utf8(&content).ok().map(|s| s.to_string()),
            Err(_) => None,
        };
        out.push(text);
    }
    Ok(out)
}

/// Build the *alive* text (keep+add only, `del`/`obs` stripped) plus a
/// map from each alive row to its display-line index, so tree-sitter
/// fold rows can be reported in display coordinates.
///
/// Display lines use the same boundaries the client derives: concatenate
/// every vertex's text, split on `'\n'`, and drop a trailing empty line.
/// Pure-ghost lines (only `del`/`obs`) are omitted from the alive text
/// but blank and alive lines are kept, so rows stay aligned with the
/// real file.
fn alive_text_and_map(vertices: &[DiffVertex]) -> (String, Vec<u32>) {
    let mut full = String::new();
    let mut alive_byte: Vec<bool> = Vec::new();
    for v in vertices {
        let is_alive = matches!(v.kind, SegmentKind::Keep | SegmentKind::Add);
        if let Some(t) = &v.text {
            full.push_str(t);
            alive_byte.resize(full.len(), is_alive);
        }
    }
    let bytes = full.as_bytes();
    // Display-line byte ranges (excluding the '\n'); a trailing newline
    // leaves no final empty line.
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            lines.push((start, i));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push((start, bytes.len()));
    }

    let mut alive_text = String::new();
    let mut map: Vec<u32> = Vec::new();
    for (di, &(lo, hi)) in lines.iter().enumerate() {
        let mut lt = String::new();
        let mut has_alive = false;
        let mut has_ghost = false;
        let mut k = lo;
        while k < hi {
            if alive_byte[k] {
                let s = k;
                while k < hi && alive_byte[k] {
                    k += 1;
                }
                lt.push_str(&full[s..k]);
                has_alive = true;
            } else {
                has_ghost = true;
                k += 1;
            }
        }
        if hi > lo && has_ghost && !has_alive {
            continue; // pure ghost line: not part of the real file
        }
        if !map.is_empty() {
            alive_text.push('\n');
        }
        map.push(di as u32);
        alive_text.push_str(&lt);
    }
    (alive_text, map)
}

// ── Reference renderer ─────────────────────────────────────────────────────

/// Render a segment stream as HTML `<span>`s with the classes
/// `seg-keep`, `seg-add`, `seg-del`, `seg-obs`. This is the reference
/// renderer proving the segment model needs no editor; real UIs (the
/// CodeMirror adapter, PDF export…) consume [`Segment`]s directly.
pub fn to_html(segments: &[Segment]) -> String {
    let mut out = String::new();
    for s in segments {
        let class = match s.kind {
            SegmentKind::Keep => "seg-keep",
            SegmentKind::Add => "seg-add",
            SegmentKind::Del => "seg-del",
            SegmentKind::Obs => "seg-obs",
            SegmentKind::Skip => "seg-skip",
        };
        out.push_str(&format!(
            "<span class=\"{}\">{}</span>",
            class,
            html_escape(&s.text)
        ));
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ── Annotated output sink ──────────────────────────────────────────────────

/// Collects the graph's output order **without fetching any content**:
/// `output_line` is handed the vertex key for every line, and we simply
/// record it plus the annotation computed earlier. The content-fetch
/// closure is never called, so `output_graph` opens no patch files —
/// `change_diff` fetches content afterwards, for the windowed rows only.
struct VBuf<'a> {
    rows: Vec<(Vertex<ChangeId>, RowKind)>,
    annotations: &'a HashMap<Vertex<ChangeId>, Ann>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Keep,
    Add,
    Del,
    Obs,
}

impl<'a> VertexBuffer for VBuf<'a> {
    fn output_line<E, F>(&mut self, key: Vertex<ChangeId>, _contents: F) -> Result<(), E>
    where
        E: From<std::io::Error>,
        F: FnOnce(&mut [u8]) -> Result<(), E>,
    {
        if key.end <= key.start {
            return Ok(());
        }
        let kind = match self.annotations.get(&key) {
            Some(Ann::Add) => RowKind::Add,
            Some(Ann::Del) => RowKind::Del,
            Some(Ann::Obs) => RowKind::Obs,
            None => RowKind::Keep,
        };
        self.rows.push((key, kind));
        Ok(())
    }

    fn output_conflict_marker<C>(
        &mut self,
        _s: &str,
        _n: usize,
        _c: Option<(&C, &[&Hash])>,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

#[derive(Debug)]
struct G {
    g: Graph,
    al: HashMap<Vertex<ChangeId>, usize>,
}

#[derive(Debug)]
enum Ann {
    Add,
    Del,
    Obs,
}

// ── Reinserting deleted vertices into the alive graph ──────────────────────

fn reinsert_deleted<T: ChannelTxnT + GraphTxnT>(
    txn: &T,
    ch: &T::Channel,
    annotations: &mut HashMap<Vertex<ChangeId>, Ann>,
    from: Option<Position<ChangeId>>,
    del: Vertex<ChangeId>,
    id: ChangeId,
    g: &mut G,
) -> Result<(), anyhow::Error> {
    let mut alive = HashSet::new();
    let gg = g.g.lines.len();
    if insert_deleted(txn, ch, annotations, del, from.is_some(), g)? {
        if let Some(from) = from {
            find_alive_up(txn, txn.graph(ch), &mut alive, from, id)?;
        } else {
            for e in iter_adjacent(txn, txn.graph(ch), del, EdgeFlags::PARENT, EdgeFlags::all())? {
                let e = e?;
                if e.flag().contains(EdgeFlags::PARENT) {
                    find_alive_up(txn, txn.graph(ch), &mut alive, e.dest(), id)?;
                }
            }
        }
        for al in alive.drain() {
            if let Some(&al) = g.al.get(&al) {
                g.g.lines[al].extra.push((None, VertexId(gg)));
            }
        }
        let v = *txn.find_block(txn.graph(ch), del.start_pos())?;
        find_alive_down(txn, txn.graph(ch), &mut alive, v)?;
        for al in alive.drain() {
            if let Some(&al) = g.al.get(&al) {
                g.g.children.push((None, VertexId(al)));
                g.g.lines[gg].n_children += 1;
            }
        }
    }
    Ok(())
}

// Add the deleted vertex to the graph. It may have been split by
// other patches since this one was applied, so walk all its blocks.
fn insert_deleted<T: ChannelTxnT + GraphTxnT>(
    txn: &T,
    ch: &T::Channel,
    annotations: &mut HashMap<Vertex<ChangeId>, Ann>,
    del: Vertex<ChangeId>,
    is_del: bool,
    g: &mut G,
) -> Result<bool, anyhow::Error> {
    let start = del.start_pos();
    let mut vertex = *txn.find_block(txn.graph(ch), start)?;
    let mut inserted = false;
    loop {
        let i = g.g.lines.len();
        annotations.insert(vertex, if is_del { Ann::Del } else { Ann::Obs });
        if g.al.insert(vertex, i).is_some() {
            if vertex.end < del.end {
                vertex = *txn.find_block(txn.graph(ch), vertex.end_pos())?;
                continue;
            } else {
                break;
            }
        }
        let mut v = AliveVertex::new(vertex);
        v.children = g.g.children.len();
        v.n_children = 1;
        g.g.children.push((None, VertexId::DUMMY));
        inserted = true;
        if vertex.end < del.end {
            v.n_children += 1;
            g.g.children.push((None, VertexId(i + 1)));
            g.g.lines.push(v);
            vertex = *txn.find_block(txn.graph(ch), vertex.end_pos())?;
        } else {
            g.g.lines.push(v);
            break;
        }
    }
    Ok(inserted)
}

fn find_alive_up<T: GraphTxnT>(
    txn: &T,
    channel: &T::Graph,
    alive: &mut HashSet<Vertex<ChangeId>>,
    pos: Position<ChangeId>,
    change: ChangeId,
) -> Result<(), anyhow::Error> {
    let mut stack = vec![SerializedEdge::empty(pos, ChangeId::ROOT)];
    let mut visited = HashSet::new();
    while let Some(elt) = stack.pop() {
        if elt.dest().is_root() {
            continue;
        }
        if !visited.insert(elt.dest()) {
            continue;
        }
        let vertex = *txn.find_block_end(channel, elt.dest())?;
        let elt_index = stack.len();
        // "is_file" here means: has non-FOLDER descendants.
        let mut is_file = false;

        let mut it = iter_adj_all(txn, channel, vertex)?;
        while let Some(v) = it.next() {
            let v = v?;
            if !v.flag().is_parent() {
                is_file |= !v.flag().is_folder();
                continue;
            }
            if v.flag() & (EdgeFlags::PSEUDO | EdgeFlags::FOLDER) == EdgeFlags::PSEUDO {
                continue;
            }
            // Here, v is a parent edge, non-pseudo.
            if !v.flag().is_deleted() {
                if v.flag().is_folder() {
                    // A folder edge: drain the iterator, checking
                    // whether `vertex` is a file; files count as
                    // alive. This includes the traversal's starting
                    // vertex (`end_pos == pos`): when the deleted text
                    // sat at the very top of the file (first line, or
                    // the whole content replaced), its only alive
                    // ancestor is the file's root vertex, and skipping
                    // it would leave the deleted text unanchored — it
                    // would silently vanish from the diff.
                    for e in it {
                        let e = e?;
                        is_file |= !e.flag().intersects(EdgeFlags::FOLDER | EdgeFlags::PARENT)
                    }
                    if is_file {
                        alive.insert(vertex);
                        stack.truncate(elt_index);
                    }
                    break;
                } else if v.flag().is_block() || vertex.is_empty() {
                    // A block edge (or an empty vertex): `vertex` is
                    // alive; stop this path, abandoning the ancestors
                    // pushed for it.
                    alive.insert(vertex);
                    stack.truncate(elt_index);
                    break;
                }
                // Otherwise, fall through and push `v`.
            } else if v.introduced_by() == change {
                // Deleted by this very patch.
                alive.insert(vertex);
                stack.truncate(elt_index);
                break;
            }
            if v.flag().is_folder() {
                // A deleted folder parent edge: same as above — accept
                // the file root even when the traversal started on it.
                if is_file {
                    alive.insert(vertex);
                    stack.truncate(elt_index);
                }
                break;
            } else {
                stack.push(*v)
            }
        }
    }
    Ok(())
}

fn find_alive_down<T: GraphTxnT>(
    txn: &T,
    channel: &T::Graph,
    alive: &mut HashSet<Vertex<ChangeId>>,
    vertex0: Vertex<ChangeId>,
) -> Result<(), anyhow::Error> {
    let mut stack = vec![SerializedEdge::empty(vertex0.start_pos(), ChangeId::ROOT)];
    let mut visited = HashSet::new();
    while let Some(elt) = stack.pop() {
        if !visited.insert(elt.dest()) {
            continue;
        }
        let vertex = *txn.find_block(channel, elt.dest())?;
        let elt_index = stack.len();
        for v in iter_adj_all(txn, channel, vertex)? {
            let v = v?;
            if v.flag().contains(EdgeFlags::FOLDER) {
                continue;
            }
            if v.flag().contains(EdgeFlags::PARENT) {
                if (v.flag().contains(EdgeFlags::BLOCK) || vertex.is_empty())
                    && !v.flag().contains(EdgeFlags::DELETED)
                    && !v.flag().contains(EdgeFlags::PSEUDO)
                {
                    if vertex == vertex0 {
                        return Ok(());
                    } else {
                        alive.insert(vertex);
                        stack.truncate(elt_index);
                        break;
                    }
                } else {
                    continue;
                }
            }
            stack.push(*v)
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pijul_core::changestore::filesystem::FileSystem as Changes;
    use pijul_core::working_copy::memory::Memory;
    use pijul_core::working_copy::WorkingCopy;
    use std::io::Write;

    /// Record the current state of `wc` on `channel` and apply it.
    fn commit(
        pristine: &Pristine,
        changes: &Changes,
        wc: &Memory,
        channel: &str,
        message: &str,
    ) -> Hash {
        let txn = pristine.arc_txn_begin().unwrap();
        let ch = {
            let mut t = txn.write();
            t.open_or_create_channel(&small(channel)).unwrap()
        };
        let mut builder = pijul_core::RecordBuilder::new();
        builder
            .record(
                txn.clone(),
                pijul_core::Algorithm::default(),
                false,
                &regex::bytes::Regex::new(r"\s").unwrap(),
                ch.clone(),
                wc,
                changes,
                "",
                1,
            )
            .unwrap();
        let mut recorded = builder.finish();
        let mut hdr = pijul_core::change::ChangeHeader::default();
        hdr.message = message.to_string();
        let hash = {
            let mut t = txn.write();
            let updates = recorded.take_updatables();
            let mut change = recorded.into_change(&*t, &ch, hdr).unwrap();
            let hash = changes
                .save_change(&mut change, |_, _| Ok::<_, anyhow::Error>(()))
                .unwrap();
            t.apply_local_change(&ch, &change, &hash, &updates).unwrap();
            hash
        };
        txn.commit().unwrap();
        hash
    }

    fn write_file(wc: &Memory, path: &str, contents: &str) {
        let mut w = wc.write_file(path, pijul_core::Inode::ROOT).unwrap();
        w.write_all(contents.as_bytes()).unwrap();
        w.flush().unwrap();
    }

    fn track(pristine: &Pristine, path: &str) {
        let txn = pristine.arc_txn_begin().unwrap();
        {
            let mut t = txn.write();
            use pijul_core::MutTxnTExt;
            t.add_file(path, 0).unwrap();
        }
        txn.commit().unwrap();
    }

    /// Build a three-commit history on an ordinary (Memory) working
    /// copy: add, insert " brave new", delete " brave". Returns
    /// (pristine, changes, dir, hash of the middle change).
    fn fixture() -> (Pristine, Changes, tempfile::TempDir, Hash) {
        let dir = tempfile::tempdir().unwrap();
        let changes = Changes::from_root(dir.path(), 1024);
        let pristine = Pristine::new_anon().unwrap();
        let wc = Memory::new();

        wc.add_file("a.md", b"hello world\n".to_vec());
        track(&pristine, "a.md");
        commit(&pristine, &changes, &wc, "main", "add a.md");

        write_file(&wc, "a.md", "hello brave new world\n");
        let hash_a = commit(&pristine, &changes, &wc, "main", "brave new");

        write_file(&wc, "a.md", "hello new world\n");
        commit(&pristine, &changes, &wc, "main", "unbrave");

        (pristine, changes, dir, hash_a)
    }

    fn kinds(resp: &ContextResponse) -> Vec<(SegmentKind, &str)> {
        resp.files
            .iter()
            .flat_map(|d| &d.vertices)
            .map(|v| (v.kind, v.text.as_deref().unwrap_or("")))
            .collect()
    }

    /// The three colours on a normal repository: viewing the middle
    /// change shows Add (still alive), Obs (added then removed by the
    /// later change), and the untouched context as Keep.
    #[test]
    fn three_colours_in_context() {
        let (pristine, changes, _dir, hash_a) = fixture();
        let diffs = change_diff(&pristine, &changes, "main", hash_a).unwrap();
        assert!(diffs.full, "small fixture must be sent whole");
        assert_eq!(diffs.files.len(), 1);
        assert_eq!(diffs.files[0].path, "a.md");
        let segs = kinds(&diffs);
        assert!(
            segs.iter()
                .any(|(k, t)| *k == SegmentKind::Obs && t.contains("brave")),
            "added-then-removed must be Obs, got {segs:?}"
        );
        assert!(
            segs.iter()
                .any(|(k, t)| *k == SegmentKind::Add && t.contains("new")),
            "still-alive addition must be Add, got {segs:?}"
        );
        assert!(
            segs.iter()
                .any(|(k, t)| *k == SegmentKind::Keep && t.contains("hello")),
            "untouched text must be Keep context, got {segs:?}"
        );
    }

    /// The Nest scenario: a *bare* pristine (fresh, empty tree tables,
    /// never any working copy) into which the changes are merely
    /// applied. The diff must render identically — paths and all —
    /// from the graph alone.
    #[test]
    fn works_on_a_bare_repository() {
        let (_pristine, changes, _dir, hash_a) = fixture();

        let bare = Pristine::new_anon().unwrap();
        {
            let txn = bare.arc_txn_begin().unwrap();
            {
                let mut t = txn.write();
                let ch = t.open_or_create_channel(&small("main")).unwrap();
                let mut chw = ch.write();
                // Pull everything (dependencies resolved recursively).
                t.apply_change_rec(&changes, &mut chw, &hash_a).unwrap();
            }
            txn.commit().unwrap();
        }
        // Sanity: really bare — no tree entries.
        {
            let txn = bare.txn_begin().unwrap();
            use pijul_core::TreeTxnT;
            assert!(
                txn.iter_tree(
                    &pijul_core::OwnedPathId {
                        parent_inode: pijul_core::Inode::ROOT,
                        basename: pijul_core::small_string::SmallString::new(),
                    },
                    None
                )
                .unwrap()
                .next()
                .is_none(),
                "fixture is not bare"
            );
        }

        let diffs = change_diff(&bare, &changes, "main", hash_a).unwrap();
        assert_eq!(
            diffs.files.len(),
            1,
            "bare repo must render the diff: {diffs:?}"
        );
        assert_eq!(
            diffs.files[0].path, "a.md",
            "path must resolve from the graph"
        );
        let segs = kinds(&diffs);
        assert!(
            segs.iter()
                .any(|(k, t)| *k == SegmentKind::Add && t.contains("brave")),
            "on the bare repo (later change not applied), 'brave' is still alive: {segs:?}"
        );
    }

    /// Build a 20-line file, one line per commit (so each line is its
    /// own graph vertex in its own patch), then one change editing the
    /// first and last lines (far apart). Returns
    /// (pristine, changes, dir, hash of the editing change).
    fn far_edits_fixture() -> (Pristine, Changes, tempfile::TempDir, Hash) {
        let dir = tempfile::tempdir().unwrap();
        let changes = Changes::from_root(dir.path(), 1024);
        let pristine = Pristine::new_anon().unwrap();
        let wc = Memory::new();

        wc.add_file("a.md", b"line 0\n".to_vec());
        track(&pristine, "a.md");
        commit(&pristine, &changes, &wc, "main", "line 0");
        let mut text = String::from("line 0\n");
        for i in 1..20 {
            text.push_str(&format!("line {i}\n"));
            write_file(&wc, "a.md", &text);
            commit(&pristine, &changes, &wc, "main", &format!("line {i}"));
        }

        let edited = text
            .replace("line 0\n", "line 0 EDIT\n")
            .replace("line 19\n", "line 19 EDIT\n");
        write_file(&wc, "a.md", &edited);
        let hash = commit(&pristine, &changes, &wc, "main", "edit ends");
        (pristine, changes, dir, hash)
    }

    /// A deleted file has no live name, so it is named by the path its
    /// deletion hunk recorded rather than by its live parent directory.
    #[test]
    fn deleted_file_keeps_its_full_path() {
        let dir = tempfile::tempdir().unwrap();
        let changes = Changes::from_root(dir.path(), 1024);
        let pristine = Pristine::new_anon().unwrap();
        let wc = Memory::new();
        wc.add_file("src/gone.md", b"bye\n".to_vec());
        wc.add_file("src/keep.md", b"stay\n".to_vec());
        track(&pristine, "src/gone.md");
        track(&pristine, "src/keep.md");
        commit(&pristine, &changes, &wc, "main", "add");
        wc.remove_path("src/gone.md", false).unwrap();
        let hash = commit(&pristine, &changes, &wc, "main", "delete");

        let diffs = change_diff(&pristine, &changes, "main", hash).unwrap();
        let paths: Vec<&str> = diffs.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/gone.md"]);
        assert!(kinds(&diffs)
            .iter()
            .any(|(k, t)| *k == SegmentKind::Del && t.contains("bye")));
    }

    /// Above the eager threshold, unchanged vertices far from any edit
    /// are sent *without* text (their content is never fetched, so the
    /// patches that contributed them are not opened); the client
    /// lazy-fetches them. The full vertex list is still sent so the
    /// client knows the file's structure.
    #[test]
    fn far_unchanged_vertices_are_lazy() {
        let (pristine, changes, _dir, hash) = far_edits_fixture();

        // Force the lazy path with a zero threshold.
        let diffs = change_diff_(&pristine, &changes, "main", hash, 0).unwrap();
        assert!(!diffs.full, "zero threshold must force the lazy path");
        assert_eq!(diffs.files.len(), 1);
        let vs = &diffs.files[0].vertices;
        assert!(
            vs.iter().any(|v| v.text.is_none()),
            "far unchanged vertices must be sent without text: {vs:?}"
        );
        let shown: String = vs.iter().filter_map(|v| v.text.as_deref()).collect();
        assert!(shown.contains("EDIT"), "edits must be shown: {shown:?}");
        assert!(
            !shown.contains("line 10"),
            "collapsed interior must not be fetched: {shown:?}"
        );
        // The structure is complete: a vertex whose text is line 10 is
        // present (just without its text) and can be lazy-fetched.
        assert!(
            vs.iter().any(|v| v.text.is_none()),
            "structure must be complete"
        );
    }

    /// Under the threshold, the whole file is sent with every vertex's
    /// text, so the client has a complete copy (interior included).
    #[test]
    fn small_file_is_sent_whole() {
        let (pristine, changes, _dir, hash) = far_edits_fixture();
        let diffs = change_diff(&pristine, &changes, "main", hash).unwrap();
        assert!(diffs.full, "small file must be sent whole");
        let joined: String = diffs.files[0]
            .vertices
            .iter()
            .filter_map(|v| v.text.as_deref())
            .collect();
        assert!(joined.contains("line 10"), "interior present: {joined:?}");
        assert!(
            diffs.files[0].vertices.iter().all(|v| v.text.is_some()),
            "every vertex must carry text in full mode"
        );
    }

    /// `vertex_contents` fetches the text of a vertex referenced by
    /// (change, start, end) directly, without a graph walk.
    #[test]
    fn vertex_contents_fetches_lazily() {
        let (pristine, changes, _dir, hash) = far_edits_fixture();
        // Get the whole structure, then re-fetch one shown vertex's text
        // via the lazy path and check it round-trips.
        let diffs = change_diff(&pristine, &changes, "main", hash).unwrap();
        let v = diffs.files[0]
            .vertices
            .iter()
            .find(|v| v.text.as_deref().is_some_and(|t| t.contains("line 5")))
            .expect("a vertex containing 'line 5'");
        let refs = vec![(
            Hash::from_base32(v.change.as_bytes()).unwrap(),
            v.start,
            v.end,
        )];
        let got = vertex_contents(&pristine, &changes, &refs).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].as_deref(), v.text.as_deref());
    }

    /// A change to a recognised language (`.rs`) carries a nested
    /// tree-sitter fold tree, computed over the alive file (the edit's
    /// `del`/`obs` ghosts stripped), in display-line coordinates.
    #[test]
    fn tree_sitter_folds_for_rust() {
        let dir = tempfile::tempdir().unwrap();
        let changes = Changes::from_root(dir.path(), 1024);
        let pristine = Pristine::new_anon().unwrap();
        let wc = Memory::new();
        let src = "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n\nfn helper(n: i32) -> i32 {\n    let mut total = 0;\n    for i in 0..n {\n        total += i;\n    }\n    total\n}\n";
        wc.add_file("a.rs", src.as_bytes().to_vec());
        track(&pristine, "a.rs");
        commit(&pristine, &changes, &wc, "main", "add a.rs");
        let edited = src.replace("let x = 1;", "let x = 10;");
        write_file(&wc, "a.rs", &edited);
        let hash = commit(&pristine, &changes, &wc, "main", "edit x");

        let resp = change_diff(&pristine, &changes, "main", hash).unwrap();
        assert!(resp.full);
        let f = &resp.files[0];
        assert_eq!(f.path, "a.rs");
        let folds = f.folds.as_ref().expect("rust file must carry folds");
        assert!(!folds.is_empty(), "expected folds: {folds:?}");
        // `helper` is a multi-line fn with a nested `for` loop.
        assert!(
            folds.iter().any(|fd| !fd.children.is_empty()),
            "expected nested folds: {folds:?}"
        );
        // Every child is contained within its parent's range.
        fn contained(fd: &crate::folds::FoldNode) {
            for c in &fd.children {
                assert!(
                    c.start >= fd.start && c.end <= fd.end,
                    "child {}-{} not within parent {}-{}",
                    c.start,
                    c.end,
                    fd.start,
                    fd.end
                );
                contained(c);
            }
        }
        for fd in folds {
            contained(fd);
        }
    }

    #[test]
    fn html_renderer_escapes() {
        let segs = vec![
            Segment {
                kind: SegmentKind::Keep,
                text: "a < b".into(),
            },
            Segment {
                kind: SegmentKind::Add,
                text: "& more".into(),
            },
        ];
        assert_eq!(
            to_html(&segs),
            "<span class=\"seg-keep\">a &lt; b</span><span class=\"seg-add\">&amp; more</span>"
        );
    }
}
