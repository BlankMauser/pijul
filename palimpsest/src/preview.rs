//! The record preview: the same segment stream as [`crate::change_diff`],
//! but for an *uncommitted* record — what a change would look like
//! before validating it.

use pijul_core::change::{Atom, Hunk as PijulHunk, LocalByte};
use pijul_core::changestore::ChangeStore;
use pijul_core::pristine::{ChangeId, EdgeFlags, GraphTxnT};

use crate::{Segment, SegmentKind};

/// Turn the recorded (uncommitted) actions into a full-context segment
/// stream over the new content: `Keep` for untouched spans, `Add` for
/// inserted spans (sliced from the new content), `Del` for removed
/// text (read from the change store, since it isn't present in the new
/// content). Fresh records never produce `Obs` — that only arises when
/// viewing a committed change ([`crate::change_diff`]).
///
/// `content` is the current ("new") state of the recorded file.
pub fn record_preview<T: GraphTxnT, C: ChangeStore>(
    txn: &T,
    changes: &C,
    recorded: &pijul_core::record::Recorded,
    content: &str,
) -> anyhow::Result<Vec<Segment>> {
    record_preview_actions(txn, changes, &recorded.actions, content)
}

/// Like [`record_preview`], but over an explicit slice of recorded actions
/// (e.g. one file's worth, grouped by the caller) rather than a whole
/// [`pijul_core::record::Recorded`]. A multi-file caller must group by file
/// and call this once per file, because each action's byte offsets index
/// into *that* file's `content`.
pub fn record_preview_actions<T: GraphTxnT, C: ChangeStore>(
    txn: &T,
    changes: &C,
    actions: &[PijulHunk<Option<ChangeId>, LocalByte>],
    content: &str,
) -> anyhow::Result<Vec<Segment>> {
    // A newly-tracked file records its entire content as a `FileAdd`
    // (not `Edit` hunks). Surface the whole document as an addition so
    // the first commit shows a non-empty diff.
    if actions
        .iter()
        .any(|h| matches!(h, PijulHunk::FileAdd { .. }))
    {
        if content.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![Segment {
            kind: SegmentKind::Add,
            text: content.to_owned(),
        }]);
    }

    enum Ev {
        Ins { start: usize, end: usize },
        Del { start: usize, text: String },
    }
    let mut evs: Vec<Ev> = Vec::new();

    for hunk in actions.iter() {
        // Both `Edit` and `Replacement` are content changes scoped by
        // `local`. A `Replacement` is a delete+insert at one spot; it
        // carries two atoms — the removal (`change`) and the new text
        // (`replacement`) — and must be processed like an `Edit`, or
        // the preview comes back empty.
        let (atoms, local): (Vec<&Atom<_>>, _) = match hunk {
            PijulHunk::Edit { change, local, .. } => (vec![change], local),
            PijulHunk::Replacement {
                change,
                replacement,
                local,
                ..
            } => (vec![change, replacement], local),
            _ => continue,
        };
        let start = local.byte.unwrap_or(0);
        for change in atoms {
            match change {
                Atom::NewVertex(v) => {
                    evs.push(Ev::Ins {
                        start,
                        end: start + (v.end - v.start) as usize,
                    });
                }
                Atom::EdgeMap(e) => {
                    for edge in &e.edges {
                        if edge.flag.contains(EdgeFlags::DELETED) {
                            let to = edge.to.unwrap();
                            let mut buf = vec![0u8; to.end - to.start];
                            changes
                                .get_contents(
                                    |h| txn.get_external(&h).ok().map(|x| x.into()),
                                    to,
                                    &mut buf,
                                )
                                .map_err(|e| anyhow::anyhow!("get_contents: {e}"))?;
                            evs.push(Ev::Del {
                                start,
                                text: String::from_utf8_lossy(&buf).into_owned(),
                            });
                        } else {
                            evs.push(Ev::Ins {
                                start,
                                end: start + (edge.to.end - edge.to.start) as usize,
                            });
                        }
                    }
                }
            }
        }
    }

    // Nothing added or deleted → empty diff, so the UI can show "no
    // changes" and disable Commit, rather than letting the user record
    // an empty patch.
    if evs.is_empty() {
        return Ok(Vec::new());
    }

    // Order by position; at the same position show the deletion before
    // the replacement insertion.
    evs.sort_by_key(|e| match e {
        Ev::Del { start, .. } => (*start, 0),
        Ev::Ins { start, .. } => (*start, 1),
    });

    let bytes = content.as_bytes();
    let mut segs: Vec<Segment> = Vec::new();
    let mut cur = 0usize;
    let keep = |segs: &mut Vec<Segment>, sl: &[u8]| {
        if !sl.is_empty() {
            segs.push(Segment {
                kind: SegmentKind::Keep,
                text: String::from_utf8_lossy(sl).into_owned(),
            });
        }
    };
    for ev in evs {
        match ev {
            Ev::Ins { start, end } => {
                if start > cur {
                    if let Some(sl) = bytes.get(cur..start) {
                        keep(&mut segs, sl);
                    }
                    cur = start;
                }
                if let Some(sl) = bytes.get(start..end) {
                    segs.push(Segment {
                        kind: SegmentKind::Add,
                        text: String::from_utf8_lossy(sl).into_owned(),
                    });
                }
                cur = end.max(cur);
            }
            Ev::Del { start, text } => {
                if start > cur {
                    if let Some(sl) = bytes.get(cur..start) {
                        keep(&mut segs, sl);
                    }
                    cur = start;
                }
                segs.push(Segment {
                    kind: SegmentKind::Del,
                    text,
                });
            }
        }
    }
    if let Some(sl) = bytes.get(cur..) {
        keep(&mut segs, sl);
    }
    Ok(segs)
}
