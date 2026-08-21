# palimpsest

A *palimpsest* is a manuscript written over scraped-off earlier text,
the old strokes still showing through. This crate renders a
[Pijul](https://pijul.org) change the same way: the whole file, with
the change inlined in its context, in three colours —

- **added** — text the change introduced, still alive;
- **removed** — text it deleted, shown in place;
- **added, then removed** — text it introduced that a *later* change
  deleted (the scraped-off layer).

The output is a flat, renderer-agnostic stream of
`Segment { kind: Keep | Add | Del | Obs, text }` per file; `to_html`
is a reference renderer, and any UI (CodeMirror decorations, PDF
export, a terminal pager…) can colour it. The companion npm package
`palimpsest-svelte` provides ready-made Svelte renderers.

```rust
let diffs = palimpsest::change_diff(&pristine, &changes, "main", hash)?;
for file in diffs {
    println!("{}", file.path);
    println!("{}", palimpsest::to_html(&file.segments));
}
```

Two entry points:

- `change_diff` — a **committed** change against the current state of
  a channel (full graph walk, including the added-then-removed layer);
- `record_preview` — a **recorded but not yet committed** change (what
  a commit dialog shows).

Both need only a pristine and a change store (any `ChangeStore`
implementation): file paths are resolved from the channel graph, so
this works on **bare** repositories — no working copy required.

## License and contributions

Dual-licensed **AGPL-3.0-or-later OR GPL-2.0-or-later**: you may use it
under either licence, at your option. The GPL-2.0-or-later arm lets it
be linked into the (GPL-2.0) Pijul CLI; standalone/network use can rely
on the AGPL arm. The author also uses this library in proprietary
products under separate terms; to keep that possible, contributions are
only accepted with copyright assignment to the author.
