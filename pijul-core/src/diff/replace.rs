use super::algorithm::*;
use super::vertex_buffer::{ConflictMarker, Diff};
use super::{Line, bytes_len, bytes_pos};
use crate::change::{Atom, Hunk, LocalByte, NewVertex};
use crate::pristine::{ChangeId, ChangePosition, EdgeFlags, Inode, Position};
use crate::record::Recorded;
use crate::text_encoding::Encoding;
use crate::{HashMap, HashSet};

#[derive(Debug)]
pub struct ConflictContexts {
    pub up: HashMap<usize, ChangePosition>,
    pub side_ends: HashMap<usize, Vec<ChangePosition>>,
    pub active: HashSet<usize>,
    pub reorderings: HashMap<usize, ChangePosition>,
}

impl ConflictContexts {
    pub fn new() -> Self {
        ConflictContexts {
            side_ends: HashMap::default(),
            up: HashMap::default(),
            active: HashSet::default(),
            reorderings: HashMap::default(),
        }
    }
}

/// Whether `line` is one of Pijul's conflict markers — `>>>>>>> N`, `======= N`
/// or `<<<<<<< N` — as written by [`crate::vertex_buffer`]. `line` still carries
/// its trailing separator. Requiring the full `<marker> <id>` shape (seven
/// identical marker bytes, a space, then a decimal id) keeps stray
/// `>>>>>>>`-looking prose from being mistaken for a marker.
///
/// A live conflict is re-emitted by the graph and handled as structure, so its
/// markers never reach recorded *content*. When one does, the file carries
/// orphaned markers (e.g. a conflict whose other side was a pending patch that
/// has since been unrecorded and dropped) and recording it would bake the marker
/// text in — which the caller refuses unless explicitly accepted.
pub(crate) fn is_conflict_marker_line(line: &[u8]) -> bool {
    let Some((&head, rest)) = line.split_first() else {
        return false;
    };
    if !matches!(head, b'>' | b'<' | b'=') || rest.len() < 8 {
        return false;
    }
    if rest[..6].iter().any(|&c| c != head) || rest[6] != b' ' {
        return false;
    }
    rest[7..]
        .iter()
        .take_while(|&&c| c != b' ' && c != b'\n')
        .all(|c| c.is_ascii_digit())
        && rest[7].is_ascii_digit()
}

impl Recorded {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn replace(
        &mut self,
        diff: &Diff,
        conflict_contexts: &mut ConflictContexts,
        lines_a: &[Line],
        lines_b: &[Line],
        inode: Inode,
        dd: &D,
        r: usize,
        encoding: &Option<Encoding>,
    ) {
        let old = dd[r].old;
        let old_len = dd[r].old_len;
        let from_new = dd[r].new;
        let len = dd[r].new_len;
        let up_context = get_up_context(diff, conflict_contexts, lines_a, old);
        debug!("up_context {:?}", up_context);
        let start = self.contents.lock().len();

        let down_context = get_down_context(
            diff,
            conflict_contexts,
            dd,
            lines_a,
            lines_b,
            old,
            old_len,
            from_new,
            len,
            start,
        );

        debug!("old {:?}..{:?}", old, old + old_len);
        trace!("old {:?}", &lines_a[old..(old + old_len)]);
        debug!("new {:?}..{:?}", from_new, from_new + len);
        trace!("new {:?}", &lines_b[from_new..(from_new + len)]);

        let mut contents = self.contents.lock();
        for &line in &lines_b[from_new..(from_new + len)] {
            // A conflict marker reaching recorded content means the working copy
            // carries orphaned markers the graph no longer backs (see
            // [`is_conflict_marker_line`]). Flag the file so the caller can refuse
            // the record unless conflict markers were explicitly accepted; the
            // content is still emitted so `--accept-conflict-markers` records it
            // verbatim. Files are recorded one at a time, so dedup on the last.
            if is_conflict_marker_line(line.l)
                && self.conflict_marker_files.last() != Some(&diff.path)
            {
                self.conflict_marker_files.push(diff.path.clone());
            }
            contents.extend(line.l);
        }
        let end = contents.len();
        if start >= end {
            return;
        }
        contents.push(0);
        std::mem::drop(contents);

        let change = NewVertex {
            up_context,
            down_context,
            flag: EdgeFlags::BLOCK,
            start: ChangePosition(start.into()),
            end: ChangePosition(end.into()),
            inode: diff.inode,
        };
        debug!("change {:?}", change);
        if old_len > 0 {
            match self.actions.pop() {
                Some(Hunk::Edit {
                    change: c, local, ..
                }) => {
                    if local.line == from_new + 1 {
                        self.actions.push(Hunk::Replacement {
                            change: c,
                            local,
                            replacement: Atom::NewVertex(change),
                            encoding: encoding.clone(),
                        });
                        return;
                    } else {
                        self.actions.push(Hunk::Edit {
                            change: c,
                            local,
                            encoding: encoding.clone(),
                        })
                    }
                }
                Some(c) => self.actions.push(c),
                None => {}
            }
        }
        self.actions.push(Hunk::Edit {
            local: LocalByte {
                line: from_new + 1,
                path: diff.path.clone(),
                inode,
                byte: Some(bytes_pos(lines_b, from_new)),
            },
            change: Atom::NewVertex(change),
            encoding: encoding.clone(),
        });
    }
}

pub(super) fn get_up_context(
    diff: &Diff,
    conflict_contexts: &mut ConflictContexts,
    lines_a: &[Line],
    old: usize,
) -> Vec<Position<Option<ChangeId>>> {
    debug!("get_up_context {:?}", old);
    if let Some(&pos) = conflict_contexts.reorderings.get(&old) {
        return vec![Position { change: None, pos }];
    }
    let old_bytes = if old == 0 {
        return vec![diff.pos_a[0].vertex.end_pos().to_option()];
    } else if old < lines_a.len() {
        bytes_pos(lines_a, old)
    } else {
        diff.contents_a.len()
    };
    // Move up until we find a "suitable" line (possibly jumping over
    // conflict markers, which aren't "suitable").
    let mut up_context_idx = diff.last_vertex_containing(old_bytes - 1);
    let mut seen_conflict_markers = false;
    loop {
        debug!(
            "marker {:?}",
            diff.marker.get(&diff.pos_a[up_context_idx].pos)
        );
        match diff.marker.get(&diff.pos_a[up_context_idx].pos) {
            None if seen_conflict_markers => {
                return vec![diff.pos_a[up_context_idx].vertex.end_pos().to_option()];
            }
            None => {
                let change = diff.pos_a[up_context_idx].vertex.change;
                let pos = diff.pos_a[up_context_idx].vertex.start;
                let mut offset = old_bytes - diff.pos_a[up_context_idx].pos;
                // Here, in the case where one side of the conflict
                // ended with a "last line without a `\n`", the offset
                // might be off by one.
                //
                // We detect that case by testing whether the vertex
                // length is the same as the length of the "line".

                let v_end = diff.pos_a[up_context_idx].vertex.end.0;
                debug!("{:?} {:?} {:?}", pos.0, offset, v_end);
                if pos.0 + offset <= v_end {
                    //
                } else if pos.0 + offset == v_end + 1 {
                    assert_eq!(diff.contents_a[old_bytes - 1], b'\n');
                    offset -= 1
                } else {
                    panic!("{:?} {:?} {:?}", pos.0, offset, v_end);
                }
                debug!("pos {:?}", pos.0 + offset);
                return vec![Position {
                    change: Some(change),
                    pos: ChangePosition(pos.0 + offset),
                }];
            }
            Some(ConflictMarker::End) => {
                debug!("get_up_context_conflict");
                return get_up_context_conflict(diff, conflict_contexts, up_context_idx);
            }
            _ => {
                let conflict = diff.pos_a[up_context_idx].conflict;
                debug!(
                    "conflict = {:?} {:?}",
                    conflict, diff.conflict_ends[conflict]
                );
                if let Some(&pos) = conflict_contexts.up.get(&conflict) {
                    debug!("up {:?}", pos);
                    return vec![Position { change: None, pos }];
                }
                seen_conflict_markers = true;
                if diff.conflict_ends[conflict].start > 0 {
                    up_context_idx = diff.conflict_ends[conflict].start - 1
                } else {
                    return vec![diff.pos_a[0].vertex.end_pos().to_option()];
                }
            }
        }
    }
}

fn get_up_context_conflict(
    diff: &Diff,
    conflict_contexts: &mut ConflictContexts,
    mut up_context_idx: usize,
) -> Vec<Position<Option<ChangeId>>> {
    let conflict = diff.pos_a[up_context_idx].conflict;
    let conflict_start = diff.conflict_ends[conflict].start;
    let mut up_context = Vec::new();
    if let Some(up) = conflict_contexts.side_ends.get(&up_context_idx) {
        up_context.extend(up.iter().map(|&pos| Position { change: None, pos }));
    }
    let mut on = true;
    conflict_contexts.active.clear();
    conflict_contexts.active.insert(conflict);
    while up_context_idx > conflict_start {
        match diff.marker.get(&diff.pos_a[up_context_idx].pos) {
            None if on => {
                let change = diff.pos_a[up_context_idx].vertex.change;
                let pos = diff.pos_a[up_context_idx].vertex.end;
                up_context.push(Position {
                    change: Some(change),
                    pos,
                });
                on = false
            }
            Some(ConflictMarker::End) if on => {
                conflict_contexts
                    .active
                    .insert(diff.pos_a[up_context_idx].conflict);
            }
            Some(ConflictMarker::Next)
                if conflict_contexts
                    .active
                    .contains(&diff.pos_a[up_context_idx].conflict) =>
            {
                on = true
            }
            _ => {}
        }
        up_context_idx -= 1;
    }
    if up_context.is_empty() {
        // The conflict has no content vertices of its own — every side is a
        // zombie/empty vertex, so the walk above never crosses a plain (`None`
        // marker) line, leaving `up_context` empty. An insert below such a
        // conflict attaches to the real context ABOVE it, so fall back to the
        // nearest non-marker vertex above `conflict_start` (mirroring
        // `get_up_context`'s "walk above the conflict" branch) instead of the
        // old `assert!(!up_context.is_empty())` crash.
        let mut i = conflict_start;
        while i > 0 {
            i -= 1;
            if diff.marker.get(&diff.pos_a[i].pos).is_none() {
                return vec![diff.pos_a[i].vertex.end_pos().to_option()];
            }
        }
        return vec![diff.pos_a[0].vertex.end_pos().to_option()];
    }
    up_context
}

#[allow(clippy::too_many_arguments)]
pub(super) fn get_down_context(
    diff: &Diff,
    conflict_contexts: &mut ConflictContexts,
    dd: &D,
    lines_a: &[Line],
    lines_b: &[Line],
    old: usize,
    old_len: usize,
    from_new: usize,
    new_len: usize,
    current_contents_len: usize,
) -> Vec<Position<Option<ChangeId>>> {
    debug!("get down context");
    if old + old_len >= lines_a.len() {
        return Vec::new();
    }
    let mut down_context_idx = 1;
    let mut pos_bytes = if old + old_len == 0 {
        0
    } else {
        let pos_bytes = bytes_pos(lines_a, old) + bytes_len(lines_a, old, old_len);
        down_context_idx = diff.first_vertex_containing(pos_bytes);
        pos_bytes
    };
    while down_context_idx < diff.pos_a.len() {
        match diff.marker.get(&(diff.pos_a[down_context_idx].pos)) {
            Some(ConflictMarker::Begin) => {
                return get_down_context_conflict(
                    diff,
                    dd,
                    conflict_contexts,
                    lines_a,
                    lines_b,
                    from_new,
                    new_len,
                    current_contents_len,
                    down_context_idx,
                );
            }
            Some(marker) => {
                if let ConflictMarker::Next = marker {
                    let conflict = diff.pos_a[down_context_idx].conflict;
                    down_context_idx = diff.conflict_ends[conflict].end;
                }
                let b_len_bytes = bytes_len(lines_b, from_new, new_len);
                // Only record a *self* side-end when this side actually inserts
                // content (`b_len_bytes > 0`). With `b_len_bytes == 0` the side
                // adds no bytes, so `current_contents_len + 0` would be the start
                // position of the next hunk's own vertex — a self-referential
                // context that crashes on apply (find_block of a not-yet-created
                // block). This mirrors the `up.insert` guard in
                // `get_down_context_conflict`. A later `get_up_context_conflict`
                // then recomputes the real context above the empty side.
                if b_len_bytes > 0 {
                    conflict_contexts
                        .side_ends
                        .entry(down_context_idx)
                        .or_default()
                        .push(ChangePosition((current_contents_len + b_len_bytes).into()));
                }
                down_context_idx += 1
            }
            None => {
                pos_bytes = pos_bytes.max(diff.pos_a[down_context_idx].pos);
                let next_vertex_pos = if down_context_idx + 1 >= diff.pos_a.len() {
                    diff.contents_a.len()
                } else {
                    diff.pos_a[down_context_idx + 1].pos
                };
                loop {
                    match dd.is_deleted(lines_a, pos_bytes) {
                        Some(Deleted { replaced: true, .. }) => return Vec::new(),
                        Some(Deleted {
                            replaced: false,
                            next,
                        }) => pos_bytes = next,
                        None => {
                            return vec![diff.position(down_context_idx, pos_bytes).to_option()];
                        }
                    }
                    if pos_bytes >= next_vertex_pos {
                        break;
                    }
                }
                down_context_idx += 1;
            }
        }
    }
    Vec::new()
}

#[allow(clippy::too_many_arguments)]
fn get_down_context_conflict(
    diff: &Diff,
    dd: &D,
    conflict_contexts: &mut ConflictContexts,
    lines_a: &[Line],
    lines_b: &[Line],
    from_new: usize,
    new_len: usize,
    current_contents_len: usize,
    mut down_context_idx: usize,
) -> Vec<Position<Option<ChangeId>>> {
    debug!("get_down_context_conflict");
    let conflict = diff.pos_a[down_context_idx].conflict;
    let len_bytes = bytes_len(lines_b, from_new, new_len);
    debug!("len_bytes {:?}", len_bytes);
    // Only cache a *self* up-context for this conflict when the resolution
    // actually inserts content (`len_bytes > 0`). With `len_bytes == 0` the
    // enclosing `replace` pushes no bytes and bails out at `start >= end`
    // without creating a vertex, so `current_contents_len + 0` would be the
    // start position of whatever hunk comes next — a self-referential
    // up-context pointing at that hunk's own vertex, which crashes on apply
    // (find_block of a not-yet-materialised block). Leaving it unset lets a
    // later `get_up_context` recompute the real context above the conflict.
    if len_bytes > 0 {
        conflict_contexts.up.insert(
            conflict,
            ChangePosition((current_contents_len + len_bytes).into()),
        );
    }
    conflict_contexts.active.clear();
    conflict_contexts.active.insert(conflict);
    assert!(!diff.pos_a.is_empty());
    let conflict_end = diff.conflict_ends[conflict].end.min(diff.pos_a.len() - 1);
    let mut down_context = Vec::new();
    let mut on = true;
    let mut pos = diff.pos_a[down_context_idx].pos;
    loop {
        match diff.marker.get(&pos) {
            None if on => match dd.is_deleted(lines_a, pos) {
                Some(Deleted { replaced: true, .. }) => on = false,
                Some(Deleted { next, .. }) => {
                    pos = next;
                    let next_pos = if down_context_idx + 1 < diff.pos_a.len() {
                        diff.pos_a[down_context_idx + 1].pos
                    } else {
                        diff.contents_a.len()
                    };
                    if pos < next_pos {
                        continue;
                    }
                }
                None => {
                    down_context.push(diff.position(down_context_idx, pos).to_option());
                    on = false;
                }
            },
            Some(ConflictMarker::Begin) if on => {
                conflict_contexts
                    .active
                    .insert(diff.pos_a[down_context_idx].conflict);
            }
            Some(ConflictMarker::Next)
                if conflict_contexts
                    .active
                    .contains(&diff.pos_a[down_context_idx].conflict) =>
            {
                on = true
            }
            _ => {}
        }
        down_context_idx += 1;
        if down_context_idx > conflict_end {
            break;
        } else {
            pos = diff.pos_a[down_context_idx].pos
        }
    }
    down_context
}
