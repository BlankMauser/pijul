# Git import fast path: validation (2026-10-29)

Base: `c67a78cf606e4f09d0ffc24aa28ac56968b786c2`.
Tested implementation: `9c4d150948f6c328206fc8b68eb5e6ddecaad7b5`.
Branch: `baark/git-import-fast`; no Baark pin or merge is part of this work.

## Why a Git-unchanged file cannot simply be skipped

The saved ryvfs commit-29 failure contains a missing **hunk**, not merely missing
bookkeeping dependencies: `Resurrecting zombie lines in src/ws.rs:338`.
`change::dependencies` derives dependencies from actions; omitting that hunk
also omits its dependencies/extra-known entries. Applying a record can leave
zombies that the next diff against identical Git bytes resurrects.

The actual WIP commit `443debc` already contains a later settled-path attempt;
its commit message describes the earlier `s3` failure. Existing `s3b` artifacts
were identical on the original three corpora. This branch retains that approach,
clarifies its API contract, tests its state transitions, and invalidates
certificates conservatively for namespace mutations. Fresh additions introduce
only new vertices, so they invalidate their own path rather than unrelated
existing file graphs. Moves, deletions and name conflicts invalidate the whole
channel. Only a no-hunk diff certifies an unchanged path; repeated self-healing
hunks remain unsettled. Merges and cold/resumed imports do not inherit a skip
claim without certification.

## Parallel recording caveat and regression

`Builder::record` is deterministic across worker counts but not universally
identical to `record_single_thread`: it allocates new-file contents during its
walk, before emitting existing-file diffs. Ryvfs commit 11 changed byte offsets
when an existing-file edit preceded the addition of `.cargo/config.toml`.

The importer therefore uses parallel recording only for namespace-stable,
single-parent commits with modification-only Git deltas. It additionally checks
that every tracked inode is present in the current channel. Other records retain
the serial layout. This does not change the core builder's behavior for other
callers. A bounded producer prepares owned Git trees/deltas using its own
repository, while record workers use a separate mutex-protected object reader.

The extended synthetic fixture visits an existing edit before a file/directory
addition. It is red with the rejected unconditional-parallel binary (commit 33
has a different hash/state), and green with the final binary. Producer error,
early-drop/join, priority/resume order, repeated zombie and partial-walk cases
also have unit coverage.

## Exact-output gate

All five artifacts match c67a78c: commit/change order, every Git-to-channel Merkle
state, final channel log, SHA-256 of every change file, and output file
paths/modes/bytes. The timing pair also passed the same comparison.

| Input | Git HEAD | Commits | Changes | Result |
|---|---|---:|---:|---|
| ryvfs | `02c6184ddb90291b5fe1f83371129e69ae6c5db5` | 414 | 358 | identical |
| usagui | `9053315fe2484a99deb91024d5a0beeef03dbe66` | 918 | 771 | identical |
| original synthetic | `e35c3e58979e24f08e221ff400b832fa67d542b9` | 31 | 27 | identical |
| BlankMauser/pi-moonbit | `bd6a70fa2806f34527930aaaf14eba67158852f2` | 868 | 761 | identical |
| extended synthetic | `5a1baa49556480b7b4d3fb24a54765dc130c9a56` | 33 | 28 | identical |

pi-moonbit includes 141 merge commits. No HareDream conversion was run here.
This is parity with the pinned converter, not a new claim that every historical
Pijul tree perfectly represents Git's tree.

## One end-result measurement

One sequential usagui conversion per binary, baseline then final, on
duriel-linux, release builds, same Git input/logging/`--stats`, no concurrent
owned builds or parity jobs. Shared-host background work remained active; load
averages and pressure snapshots are retained. No intermediate performance
comparison or microbenchmark was used.

| Metric | c67a78c | final |
|---|---:|---:|
| Wall seconds | 507.527342 | 149.731028 |
| CPU seconds (user + system) | 504.098794 | 137.114111 |
| Physical read bytes | 1,171,726,336 | 726,835,200 |
| Logical read bytes (`rchar`) | 27,218,126,262 | 8,983,632,600 |

**3.39× wall-time speedup**, 72.8% less CPU and 38.0% fewer physical read bytes in
this pair. This is not a statistical benchmark or a speed guarantee for every
repository. CPU is from `wait4`; `waitid(WNOWAIT)` keeps the exited process
available for a final `/proc/<pid>/io` read before reaping. Physical read bytes
also exactly match `wait4.ru_inblock * 512` for both runs.

Binary SHA-256:

- c67a78c: `b95e4cd0fa941d649d916b5541a681d1d88f54cb55cfb3119a3cbb4fd65fb571`
- final: `3cba77519257886b36a3303a59df0c2551c9eed567dee9daa8796603a50c94d4`

## Tests, lint and retained evidence

- CLI tests: 6 passed. Core library: 226 passed, 4 ignored. Formatting and
  `git diff --check` pass.
- Strict Clippy is **not clean**. The CLI-only final/c67 comparison has the same
  134 existing diagnostics, normalized by lint/message/file, with no added or
  removed counts. It swaps only the importer source to isolate the CLI delta,
  keeping the dependency tree fixed. The broader check stops on 25 inherited
  core errors and 2 inherited interaction errors.
- All Cargo invocations used the Captain build lock, jobs=8, dev debug=0,
  incremental=0, Captain TMPDIR (also forced through Cargo config), and the
  dedicated `~/code-build/.target-pijul-fast` target. No protected binary was
  replaced.
- Evidence on duriel-linux: `~/.cache/pijul-fast/`. `*.final.parity` covers the
  four corpora; `mixed.refined.parity` covers the new regression;
  `measurement{,-baseline}.parity` covers the timing runs. Full inputs, outputs,
  failed-parallel evidence, build/test logs, measurement scripts and
  `usagui.measure-{base,final}/measurement.json` are retained.
- Final standalone executable: `~/.cache/pijul-fast/bin/pijul-final`.
