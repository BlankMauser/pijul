# Changelog

## Unreleased

### Fixed

- Correct handling of mtime when unrecording and when recording
  partial changes.

### Changed

- Upgraded cryptography stack: `ed25519-dalek` 2.x (fixes RUSTSEC-2022-0093
  timing vulnerability in batch verification), `sha2`/`hmac`/`pbkdf2`/`getrandom`
  to current versions.
- Upgraded `hyper` to 1.x and `reqwest` to 0.12.
- Tag file format removed from protocol; tags will move to the Git bridge.
- Changed change signature format to sshsig. This allowed us to remove
  the dependency on keyring, which caused problems when used over SSH
  on a Linux server with Wayland (no way to reach that password
  window!), and didn't even compile on OpenBSD.
- Removed interactivity, in order to make Pijul more usable in scripts.

## 1.0.0-beta.14

### Fixed

- Made the change file cache thread-safe (`RefCell` → `parking_lot::Mutex`),
  fixing potential unsoundness under concurrent output operations.

## 1.0.0-beta.13

### Fixed

- Minor internal cleanup in edge flag handling.

## 1.0.0-beta.12

### Fixed

- Fixed missing context repair to also zombify unknown *children* (not only
  unknown parents), preventing a class of corruption where downward edges were
  left dangling after applying a change on top of a partial history.
- Added `repair_up` pass after zombie repair for better graph consistency.

### Changed

- Crate renamed from `libpijul` to `pijul-core`.

## 1.0.0-beta.11

### Fixed

- Overhauled zombie conflict repair: switched to deterministic `BTreeSet`
  traversal order and added pseudo-edges to all descendants when re-visiting a
  vertex, closing a class of non-deterministic conflict output bugs.
- Improved conflict output debugging (zombie/cyclic conflict IDs now traced).
- Made `ApplyWorkspace` fields public to allow external tooling and testing.

### New features

- Added rollback tests covering unrecord-then-reapply round-trips.

## 1.0.0-beta.10

### Fixed

- Reworked missing-context repair with a new `repair_zombies` algorithm that
  tracks the last-alive vertex on each DFS path, correctly adding pseudo-edges
  to reconnect zombie content to its nearest live ancestor.
- Removed a leftover hardcoded debug vertex probe that was accidentally
  committed in beta.7.

## 1.0.0-beta.9

### Fixed

- Fixed non-termination / double-visit bug in the alive-graph DFS traversal
  (zombie detection), caused by incorrect ordering of the `visited` check and
  stack push.

## 1.0.0-beta.8

### Fixed

- Fixed a panic when `get_external` returns `None` for a parent edge's
  `introduced_by` field: now returns a `Corruption` error instead of
  unwrapping, so the error is reported cleanly rather than crashing.

## 1.0.0-beta.7

### Fixed

- Added `include_deleted` flag to the alive-graph retrieval, fixing cases where
  deleted vertices were incorrectly excluded from the conflict graph.
- Improved error propagation: `MakeChangeError` now threads through `ApplyError`
  and `LocalApplyError` correctly.

## 1.0.0-beta.6

### Fixed

- Fixed missing-context repair targeting the wrong vertices in certain topologies.
- Fixed file-move tracking: introduced a `move_map` so that chains of moves
  within a single change are applied in the correct order and intermediate
  names are resolved properly.

## 1.0.0-beta.5

### Fixed

- Fixed the text change parser to reject empty content blocks instead of
  silently accepting them, which could produce invalid changes.
- Moved `write_all_deps` to `Hashed` (the correct owner), fixing dependency
  serialization for changes that haven't been saved yet.

## 1.0.0-beta.4

### Fixed

- Fixed conflict markers to include author information from the changestore
  (previously the changestore was dropped before the marker was written).
- Fixed the text change format parser to accept `\r\n` line endings, fixing
  round-trips on Windows.
- Fixed parsing of replace/delete hunks where the `+`/`-` content direction
  was determined incorrectly when one side was empty.

## 1.0.0-beta.3

### Fixed

- Migrated apply internals to `ArcTxn<T>` / `ChannelRef<T>` shared-locking,
  preventing lock ordering issues under concurrent reads.
- `put_newvertex` now takes a `knows` closure instead of a `Change` reference,
  fixing incorrect "unknown dependency" classification in certain apply paths.

## 1.0.0-beta.2

## 1.0.0-beta.2

### Fixed

- Fixing a bug with name conflicts, where files could end up with 0 alive name.
- Fixing a few panics/unwraps
- Fixing a bug where a zombie file could be deleted by `pijul unrecord`, but its contents would stay zombie.
- CVE-2022-24713

### New features

- Better documentation for `pijul key`.
- `pijul pull` does not open $EDITOR anymore when given a list of changes.

## 1.0.0-beta.1

### Fixed

- Fixed a failed assertion in the patch text format.
- Fixed a "merged vertices" bug when moving files and editing them in the same patch, where the new name was "glued" to the new lines inside the file, causing confusion.
- Fixed a performance issue on Windows, where canonicalizing paths can cause a significant slowdown (1ms for each file).
