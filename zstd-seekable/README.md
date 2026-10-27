# zstd-seekable (Baark's patched copy)

This is zstd-seekable 0.1.23 from crates.io with one change: it links zstd
through `zstd-sys` instead of compiling its own copy of zstd's C sources. The
release's bundled zstd and `zstd-sys` define the same `ZSTD_*` symbols, so a
binary that links both (Pijul plus the `zstd` crate) fails to link, or with
`--allow-multiple-definition` silently uses one copy for both callers.

Kept from the release, byte for byte: `src/bindings.rs`, the seekable format
sources in `seekable/` (zstd's `contrib/seekable_format`), the internal
headers they need in `seekable/common/` and `xxh64.c`. `src/lib.rs` only adds
`extern crate zstd_sys`. The workspace selects it with `[patch.crates-io]`.

# zstd-seekable

Seekable compression standard developped by Facebook.
This format is expected, but not guaranteed, to be stable. Consult https://facebook.github.io/zstd/ for more detail.

