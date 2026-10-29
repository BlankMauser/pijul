# `pijul git` determinism harness

A re-conversion of the same Git history must produce byte-identical Pijul
changes (same hashes, same order, same channel states): baark.dev refuses a
re-import whose conversion differs. These scripts convert a corpus with an old
and a new `pijul` binary and compare everything the conversion leaves behind.

```sh
# once: the per-commit state dumper (standalone crate, sanakirja only)
(cd gitdb-dump && cargo build --release)   # or set GITDB_DUMP=/path/to/gitdb-dump
./corpus.sh ~/.cache/pijul-git-speed/corpus          # synthetic + ryvfs + usagui
./run-all.sh OLD_PIJUL NEW_PIJUL ~/.cache/pijul-git-speed/runs [synthetic ryvfs usagui]
```

`convert.sh PIJUL GIT_REPO OUT` clones like `ruttr import git`
(`--single-branch --no-tags`), runs `PIJUL git OUT/git OUT/pijul --stats ...`
and writes:

| file | content |
|---|---|
| `commits.txt` | per processed commit, in processing order: `n oid change-hash` (`-` when nothing was recorded) |
| `gitdb.txt` | per commit: `oid channel-Merkle` from `.pijul/git/db` (what `pijul git` resumes from) |
| `log.txt` | `pijul log --state` of the final channel |
| `changes.txt` | sha256 of every change file |
| `tree.txt` | mode + sha256 of every file of the final output |
| `run.txt` | wall seconds, `rchar`/`read_bytes` from `/proc/<pid>/io`, `ulimit -n` |

`compare.sh A B` exits 0 only if all of them are identical; otherwise it names
the first differing commit (oid, subject, parents, both hashes) and how many
commits' Merkle states differ. `STATS=0 convert.sh ...` skips `--stats` for
timing runs (no `commits.txt` then).

`make_synthetic.sh DIR` builds a deterministic repo (fixed dates, seeded
binaries) with: clean and two-sided merges, identical edits on both sides,
the same path added on both sides (equal and different contents), plain /
edited / directory / binary renames, deletions, binary add+modify, an exec-bit
change, a symlink, an empty commit, and a second root merged with
`--allow-unrelated-histories`.

The fast-path parity gates, regression diagnosis and final measurement are in
[fast-import-validation.md](fast-import-validation.md).

## Import fast-path invariants

- Git-unchanged does not mean Pijul-clean. A previous record can leave zombie
  lines that the next identical diff resurrects. Skip only paths whose last
  diff emitted no hunks; content hunks and fresh file additions invalidate that
  path. Other namespace hunks invalidate the whole channel's certificates:
  unlike a fresh addition, they may mutate existing descendants/aliases.
  Merges/resumes full-diff
  until certificates are re-established. Names/metadata are always checked.
- Parallel `Builder::record` is deterministic across worker counts, but its
  new-file allocation happens before existing-file diffs. This is **not** the
  layout of `record_single_thread` for mixed additions/edits. The importer
  retains the serial path for unknown namespaces, merges and non-modification
  Git deltas, and checks every tracked inode's channel membership before using
  parallel record. The synthetic corpus includes an existing-file edit visited
  before a new file/directory, specifically to catch changed contents offsets.
- A bounded producer owns its own Git repository and prepares owned tree/delta
  snapshots in the importer's priority order. The real importer verifies each
  OID before consuming it. Record workers share a separate mutex-protected Git
  object reader; no non-Sync git2 handle is shared unsafely. No blobs are read
  merely to populate a snapshot. Early importer errors disconnect and join the
  producer, including when its result queue is full.

Self-check: the harness was shown to catch a deliberate change (one commit's
timestamp shifted by 1 s): it reported the commit, its two hashes, and the 13
commits whose Merkle state changed downstream.
