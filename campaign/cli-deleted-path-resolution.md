---
scope: shared
title: CLI path resolution must tolerate deleted files (use get_prefix, not canonicalize)
---

# Convention: resolving user-supplied paths in CLI commands

Commands that act on a path which may no longer exist in the working copy
(`record`, `remove`, `reset`, `add --recursive`, ...) MUST resolve it with
`pijul_core::working_copy::filesystem::get_prefix`, **never** with
`std::fs::canonicalize` / `Path::canonicalize`.

`canonicalize` requires the path to exist on disk; for a file the user just
`rm`ed it fails with the opaque `No such file or directory (os error 2)` and the
command does nothing. `get_prefix` normalizes `.`/`..` logically, canonicalizes
the longest existing ancestor, and re-attaches the missing tail, yielding the
real repo-relative slash path even for a deleted file (and the empty string for
a path outside the repo).

## Pattern

```rust
let abs = if path.is_absolute() { path.clone() } else { cwd.join(path) };
let (_full, path_str) =
    pijul_core::working_copy::filesystem::get_prefix(Some(repo_path.as_ref()), &abs)?;
if path_str.is_empty() || !txn.is_tracked(&path_str)? {
    // clean error, not a raw OS error
    anyhow::bail!("path not tracked by Pijul: {}", path.display());
}
```

## History

- 2026-08-11: `record` fixed the same way ("Record a deleted file's deletion
  when its path can't be canonicalize").
- 2026-08-16: `remove` and `reset` (incl. `reset --dry-run`, 3 sites) had the
  same `canonicalize` bug — fixed; not-tracked paths now report a clear error
  instead of `os error 2`.
