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

Self-check: the harness was shown to catch a deliberate change (one commit's
timestamp shifted by 1 s): it reported the commit, its two hashes, and the 13
commits whose Merkle state changed downstream.
