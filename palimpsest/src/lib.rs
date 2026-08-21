//! A *palimpsest* is a manuscript written over scraped-off earlier
//! text, the old strokes still showing through. This crate renders a
//! [Pijul](https://pijul.org) change the same way: the whole file,
//! with the change inlined in its context —
//!
//! - [`SegmentKind::Keep`]: text untouched by the change;
//! - [`SegmentKind::Add`]: text it added, still alive;
//! - [`SegmentKind::Del`]: text it deleted;
//! - [`SegmentKind::Obs`]: text it added that a *later* change
//!   removed ("obsolete" — the scraped-off layer).
//!
//! The output is a flat, renderer-agnostic [`Segment`] stream per
//! file; [`to_html`] is a reference renderer, and any UI (CodeMirror
//! decorations, PDF export, a terminal pager…) can colour it.
//!
//! There are two producers of that stream, and they define this
//! crate's boundary: [`change_diff`] for a **committed** change (the
//! full graph walk, including [`SegmentKind::Obs`]) and
//! [`record_preview`] for a **recorded but not yet committed** one
//! (position-based, no `Obs` by construction). Both need only
//! pijul-core — no working copy, no collaboration machinery; anything
//! that requires those is out of scope for this crate.
//!
//! File paths are resolved from the channel graph itself
//! ([`pijul_core::fs::find_path`]), so this works on **bare**
//! repositories — no working copy, no tree tables required. Only a
//! pristine and a change store.
//!
//! The annotation algorithm works by re-inserting the deleted
//! vertices into pijul's alive graph before walking it, so the
//! output covers the whole file rather than isolated hunks.

mod diff;
mod folds;
mod preview;

pub use diff::*;
pub use folds::*;
pub use preview::*;
