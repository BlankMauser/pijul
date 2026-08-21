# Pijul Architecture

This document describes the internal architecture of the Pijul workspace: its crates, how they relate, the on-disk layout, the core data structures, and the step-by-step data flows for the most important operations.

See also [`architecture.svg`](architecture.svg) for a visual overview.

---

## Workspace crates

The workspace contains eight crates with a clear layering:

```
pijul              CLI binary — all user-facing commands
├── pijul-remote       Network transports (SSH, HTTP, local)
├── pijul-repository   Repo discovery and initialization
├── pijul-identity     SSH key identity management
├── pijul-config       Configuration loading (global + per-repo)
├── pijul-interaction  Progress bars and spinners
└── pijul-core         The algorithmic heart (everything below)
    └── pijul-macros   Proc-macros for DB boilerplate (build-only)
```

`pijul-core` is the foundation. Every other crate depends on it directly or indirectly. `pijul-macros` is a build-time proc-macro crate with no runtime dependencies.

---

## Crate descriptions

### `pijul-core`

Contains the entire theory of Pijul: the graph data model, diff algorithms, conflict detection, change serialization, and the database abstraction layer.

Key sub-modules:

| Module | Purpose |
|---|---|
| `pristine/` | All DB traits (`GraphTxnT`, `ChannelTxnT`, `TreeTxnT`, `DepsTxnT`, `TxnT`, `MutTxnT`), core types, and the Sanakirja backend |
| `change/` | `Change` data structure, serialization (zstd-compressed bincode + human-readable text format), Blake3 hashing |
| `changestore/` | `ChangeStore` trait, filesystem implementation (`.pijul/changes/`), in-memory implementation |
| `apply/` | Applies a `Change` to the pristine graph: inserts vertices and edges, updates inode tables, advances the Merkle state |
| `record/` | Diffs the working copy against the pristine graph to produce a new `Change`; multithreaded via work-stealing |
| `diff/` | Myers, Patience, and Histogram diff algorithms (wrapping `imara-diff`/`diffs`) |
| `output/` | Reconstructs working-copy files from the pristine graph; detects and marks conflicts |
| `alive/` | Retrieves the "alive" subgraph reachable from an inode root; runs Tarjan SCC for conflict detection |
| `unrecord/` | Inverse of `apply/` — removes a change from a channel |
| `fs/` | Inode ↔ tree path operations (add, move, remove tracked files) |
| `missing_context.rs` | Repairs "zombie" vertices by inserting `PSEUDO` edges when a deleted vertex was someone else's context |
| `working_copy/` | `WorkingCopy` trait; filesystem implementation reads actual files from disk |
| `tag/` | Named Merkle states ("tags") that snapshot a channel at a point in time |
| `key.rs` | `PublicKey` / `SKey` for change signing via SSH agent |

Feature flags: `ondisk-repos` (default), `text-changes` (default), `mmap`, `zstd`, `git2`.

---

### `pijul-macros`

A proc-macro crate that generates repetitive boilerplate for the database abstraction layer. The macros `table!`, `get!`, `cursor!`, `iter!`, `put_del!`, and `initialized_cursor!` expand to associated-type declarations and method stubs in the DB traits.

---

### `pijul-config`

Merges the tracked repository-wide `pijul.toml` (weakest), global (`~/.config/pijul/config.toml`) and per-repository personal (`.pijul/config`) configuration using `figment`. Key types:

- `Config` — the merged config object passed around by the CLI
- `Author` — name, email, and origin fields
- `Hooks` — `preHooks` and `record` hooks. The tracked `pijul.toml` may declare shared hooks (kept in `Config::shared_hooks`, out of the figment merge); being versioned code, they run only once approved via `pijul hooks approve`, the approval fingerprint (a BLAKE3 hash) living in the untracked `.pijul/config.toml`.
- `RemoteConfig` — named remotes with SSH or HTTP support and custom headers
- `Template` — commit message and description templates

---

### `pijul-identity`

Manages user identities stored in `~/.config/pijul/identities/<NAME>/identity.toml`. Each identity carries a `PublicKey` reference; private keys are never stored on disk — signing is delegated to the SSH agent.

---

### `pijul-interaction`

A thin crate providing `ProgressBar`, `Spinner` (backed by `indicatif`), and an `InteractiveContext` enum (`Terminal` vs `NotInteractive`).

---

### `pijul-repository`

Ties together the three on-disk components of a repository:

```rust
pub struct Repository {
    pub pristine:     sanakirja::Pristine,          // .pijul/pristine/
    pub changes:      changestore::FileSystem,       // .pijul/changes/
    pub working_copy: working_copy::FileSystem,      // repo root
    pub path:         PathBuf,
    pub changes_dir:  PathBuf,
}
```

Provides `Repository::find_root()` (walks up the directory tree looking for `.pijul/`) and `Repository::init()`.

---

### `pijul-remote`

The networking layer. Defines:

- `RemoteRepo` — an enum over `Local`, `Ssh`, `Http`, `LocalChannel`, and `None` transports
- `CS` — either `Change(Hash)` or `State(Merkle)`, the two transferable unit types
- `RemoteDelta<T>` — the difference between a remote's cached state and its actual current state
- `PushDelta` — `to_upload`, `remote_unrecs`, `unknown_changes`

The three transports in `local.rs`, `ssh.rs` (`thrussh`), and `http.rs` (`reqwest`) all implement the same async interface (`pull`, `upload_changes`, `download_changes`, `download_changelist`).

---

### `pijul` (CLI binary)

Entry point in `main.rs`: a `tokio::main` async runtime dispatching to ~25 commands via `clap` derive macros. Each command lives in `commands/<name>.rs`, opens a `Repository`, starts a Sanakirja transaction, and calls into `pijul-core` or `pijul-remote`.

---

## On-disk layout

A Pijul repository lives entirely under `.pijul/` inside the working copy root:

```
<repo root>/
├── .pijul/
│   ├── pristine/          Sanakirja memory-mapped database
│   │                        all channels, graph, history,
│   │                        inode tables, remote caches
│   ├── changes/
│   │   └── XX/            XX = first 2 Base32 chars of hash
│   │       └── <hash>.change   zstd-seekable bincode
│   └── config             per-repo TOML config
└── (working copy files)

~/.config/pijul/
├── config.toml            global configuration
└── identities/
    └── <NAME>/
        └── identity.toml  author fields + public key ref
```

---

## Core data structures

### `Vertex<H>`

The fundamental node in the repository graph. Represents a half-open byte range `[start, end)` within the content buffer of change `H`:

```rust
struct Vertex<H> {
    change: H,
    start:  ChangePosition,
    end:    ChangePosition,
}
```

`H` is `ChangeId` inside the database, `Hash` on the wire, or `Option<Hash>` while constructing a new change.

`Position<H> { change: H, pos: ChangePosition }` is a single byte within a change, used as inode identifiers and edge targets.

---

### `SerializedEdge`

Three packed little-endian u64 words:

```
word 0: [EdgeFlags (1 byte)] [dest.pos (7 bytes)]
word 1: dest.change  (ChangeId)
word 2: introduced_by (ChangeId — which change added this edge)
```

`EdgeFlags` bitmask: `BLOCK=1`, `PSEUDO=4`, `FOLDER=16`, `PARENT=32`, `DELETED=128`.

Both a forward edge and its reverse (`PARENT` flag) are always stored together, keeping the graph permanently bidirectional.

---

### `Hash`

```rust
enum Hash {
    None,                  // root change (sentinel)
    Blake3([u8; 32]),      // primary algorithm
    GitSha1([u8; 20]),     // for git-imported changes
    GitSha2([u8; 32]),
}
```

Displayed and stored in a custom Base32 alphabet (`A-Z2-7`).

---

### `Merkle`

```rust
enum Merkle {
    Ed25519(EdwardsPoint),  // a point on Curve25519
}
```

The channel state is advanced as each change is applied:

```
new_state = old_state.next(hash)
          = old_state * scalar(hash)
```

This is an **elliptic-curve accumulator**, not a traditional Merkle tree. Each channel carries a 33-byte fingerprint of its entire ordered change history. The `states` B-tree makes it O(log n) to ask "does this channel contain state S?"

---

### `Change`

```rust
struct LocalChange {
    offsets:  Offsets,     // byte offsets for seeking within the file
    hashed:   Hashed {     // Blake3-hashed to produce the change hash
        version:       u64,
        header:        ChangeHeader,   // message, description, timestamp, authors
        dependencies:  Vec<Hash>,
        extra_known:   Vec<Hash>,      // zombie dependency context
        metadata:      Vec<u8>,
        changes:       Vec<Hunk>,      // the actual diff operations
        contents_hash: Hash,
    },
    unhashed: Option<Value>,  // JSON: change signature lives here
    contents: Vec<u8>,        // raw byte content for NewVertex atoms
}
```

Each `Hunk` groups `Atom`s for a single inode. An `Atom` is either:

- `NewVertex { up_context, down_context, flag, start, end, inode }` — inserts new content between two existing nodes
- `EdgeMap { edges: Vec<NewEdge>, inode }` — modifies existing edges (line deletions, conflict resolutions)

---

### Sanakirja B-tree tables

**Per-channel:**

| Table | Key | Value | Purpose |
|---|---|---|---|
| `graph` | `Vertex<ChangeId>` | `SerializedEdge` | The main DAG |
| `changes` | `ChangeId` | `L64` (apply timestamp) | Which changes are in this channel |
| `revchanges` | `L64` (timestamp) | `(ChangeId, SerializedMerkle)` | Ordered log: position → (change, state) |
| `states` | `SerializedMerkle` | `L64` | Fast lookup: is this state in the channel? |
| `tags` | `L64` | `(SerializedMerkle, SerializedMerkle)` | Named channel snapshots |

**Global (across all channels):**

| Table | Purpose |
|---|---|
| `internal` / `external` | `Hash ↔ ChangeId` bidirectional map |
| `dep` / `revdep` | `ChangeId → ChangeId` dependency graph |
| `touched_files` / `rev_touched_files` | `Position ↔ ChangeId` — which changes touch which file positions |
| `tree` / `revtree` | `PathId → Inode` / `Inode → PathId` — the tracked filesystem tree |
| `inodes` / `revinodes` | `Inode ↔ Position<ChangeId>` — maps filesystem inodes to graph positions |
| `channels` | Channel name → `SerializedChannel` |
| `remotes` | `RemoteId → SerializedRemote` |

---

## Data flows

### `pijul record`

1. **Open repo** — `Repository::find_root()` locates `.pijul/`, opens Sanakirja and the change store.
2. **Load channel** — `txn.load_channel("main")` returns a `ChannelRef<T>` (`Arc<RwLock<Channel>>`).
3. **Run pre-record hooks** from config.
4. **Build record** — `RecordBuilder::record(…, n_threads)`:
   - Walks the tracked tree via `fs::iter_working_copy()`
   - Per file: reads from the working copy, retrieves the alive graph (current stored contents), runs a diff algorithm, emits `Hunk<Option<ChangeId>, LocalByte>` actions
   - Files are dispatched across worker threads via a `crossbeam-deque` work-stealing queue
5. **Globalize** — local byte offsets are converted to `Position<Option<Hash>>` references: `rec.globalize(&txn)`.
6. **Compute dependencies** — `change::dependencies()` traverses context vertices and calls `minimize_deps()` for transitive reduction.
7. **Prompt / editor** — the user edits the `ChangeHeader` and selects hunks interactively (unless `--all`).
8. **Sign** — `pijul_identity::sign_pem(&secret, &hash.to_bytes())` via the SSH agent; signature stored in `change.unhashed`.
9. **Save change** — `repo.changes.save_change(&mut change)` writes to `.pijul/changes/XX/<hash>.change` as zstd-compressed bincode and returns a `Hash` (Blake3).
10. **Apply locally** — `txn.apply_local_change(&channel, &change, &hash, &inode_updates)`:
    - Registers the change in `internal`/`external`, `dep`/`revdep`, `touched_files`
    - Inserts graph vertices and edges
    - Updates `inodes`, `revinodes`, `tree`, `revtree`
    - Appends to `revchanges`, advances the Merkle accumulator
11. **Output** — `output::output_repository_no_pending()` reconstructs working-copy files from the updated graph.
12. **Commit** — `txn.commit()` flushes Sanakirja to disk.

---

### `pijul push`

1. Open repo and channel (local side).
2. **Resolve remote** — `pijul_remote::repository(…)` → `RemoteRepo` enum variant (SSH, HTTP, or local).
3. **Get remote delta** — `remote.get_remote_delta(…)`:
   - Downloads the remote's changelist via `download_changelist()` — a sequence of `(n, Hash, Merkle)` entries
   - Compares against the local `RemoteRef` cache to find what's new
4. **Compute `PushDelta`** — `remote_delta.to_push_delta()`:
   - `to_upload`: changes in the local channel not present on the remote (in apply order)
   - `remote_unrecs`: changes the remote previously had but has since unrecorded
   - `unknown_changes`: changes on the remote we don't have locally
5. **Warn / ask** about `remote_unrecs` and `unknown_changes`.
6. **Interactive selection** — open editor with `make_changelist()` / `parse_changelist()` unless `--all`.
7. **Upload** — `remote.upload_changes(…)`:
   - `Local`: directly applies each `CS` to the remote Sanakirja database
   - `Ssh`: sends change files over the SSH channel using the Pijul binary protocol (version 3)
   - `Http`: HTTP-POSTs each change file to the server
8. **Update remote cache** — store the remote's new state in the local `RemoteRef` table.

---

### `pijul pull`

1. Open repo and channel.
2. Resolve remote → `RemoteRepo`.
3. **Get remote delta** — identifies `to_download` (changes on the remote not in the local channel).
4. **Interactive selection** of which changes to pull.
5. **`remote.pull(…)`**:
   - Spawns a Tokio task that streams changes from `remote.download_changes()`
   - `download_changes_rec()` recursively discovers and downloads dependencies first
   - Changes arrive via an `mpsc` channel as they download
   - For each: calls `txn.write().apply_change_rec_ws(changes, &mut channel, &h, &mut ws)`
6. **Output** — `output::output_repository_no_pending()` writes reconstructed files to the working copy.
7. **Commit** — `txn.commit()` flushes Sanakirja.

---

## Notable algorithms

### Conflict detection — Tarjan SCC

When outputting a file, Pijul performs a DFS from the inode's root vertex collecting all alive (non-deleted) vertices and edges into an `AliveVertex` adjacency list. This graph is then run through an iterative (non-recursive) Tarjan SCC algorithm. Any SCC of size > 1 is a **cycle** — two concurrent edits that both inserted content between the same surrounding nodes — which Pijul renders as conflict markers in the working copy file.

### Zombie conflicts — missing context

When a change deletes content that another change's context vertices depended on, those dead vertices become "zombie" — Pijul keeps them alive via synthetic `PSEUDO` edges (introduced by `missing_context.rs`) and marks them with the `ZOMBIE` flag. The `Conflict::Zombie` variant appears in output.

### Dependency minimization

`change::minimize_deps()` implements a transitive-reduction step: given the set of all changes whose vertices appear in the new change's context, it removes dependencies that are already implied by transitivity through other dependencies, by walking the `dep` graph from each dependency.

### Elliptic-curve state accumulator

The channel's Merkle state is not a hash tree. It is a point on Curve25519 that advances with each applied change:

```
state.next(hash) = state * scalar(hash)
```

where `scalar(hash)` is derived from the change's Blake3 hash. This gives a compact 33-byte commitment to the ordered change history. `last_common_state()` uses binary search over the `revchanges` log to efficiently find where two channels diverge.

### Record parallelism

`RecordBuilder::record()` uses `crossbeam-deque` for work-stealing: each tracked file is pushed as a task and worker threads pull files to diff concurrently. Each worker accumulates a `Recorded` output; they are merged into a single result at the end. The raw `contents` byte buffer is shared via `Arc<Mutex<Vec<u8>>>`.

### Change file format

Change files are stored at `.pijul/changes/<XX>/<rest>.change` where `XX` is the first two Base32 characters of the Blake3 hash. The file is zstd-seekable compressed bincode with an `Offsets` header so the `hashed` section (needed for verification) and the `contents` section (raw byte data) can each be read without decompressing the entire file.

### The Pijul binary protocol

`commands/protocol.rs` implements the server side of the protocol that runs over SSH when `pijul protocol` is invoked remotely. It exchanges change lists and streams change files. The current protocol version is `3` (`pijul_remote::PROTOCOL_VERSION`).
