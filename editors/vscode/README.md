# Pijul + Claude (VSCode)

A thin VSCode client for the `piclaude` workflow (`contrib/agents/`): run
parallel Claude agents in isolated Pijul forks, and **validate patches before
recording** by carving the pending diff into logical changes.

Same contract-first design as the emacs modes (`../emacs/`): no shared UI code,
just a translator over `pijul … --output-format json` and the
`record --from-change` carve patch. All the real logic lives in the CLI.

## What it gives you

- **Source Control panel** — pending changes from `pijul diff --output-format
  json`, one entry per file, click to open.
- **Pijul: Carve** (title-bar ✓) — the star. `pijul diff` → pick the hunks that
  form one logical change (multi-select) → message → `pijul record
  --from-change`. Loops over the rest. This is "Saveur 2" from the piclaude
  README, rendered in VSCode's native QuickPick instead of `AskUserQuestion`.
- **Pijul: Record Hunk Under Cursor** (editor context menu) — records the single
  hunk covering the cursor (`pijul diff` → that block → `record --from-change`).
  Pragmatic per-hunk staging without a gutter (see *Status*).
- **Pijul: Review Claude's Proposed Changes** — the Claude → VSCode handoff.
  Claude writes a decomposition to `.pijul-carve.json` (watched); the extension
  pops a review list (preview each, uncheck to skip) and records the approved
  groups. Same gesture as *Carve*, but Claude does the semantic split.
- **Pijul: New Agent Workspace (fork)** — `piclaude fork`, offers to open the
  workspace in a new window.
- **Pijul: Land to Main** — `piclaude land`: record in the fork, then (under a
  lock) pull main in and push back, so main stays buildable. Pick *Record* or
  *Amend last change*, give a message. On conflict it stops and tells you to
  resolve the markers in the fork and land again (it's re-runnable).
- **Pijul: Integrate Agent Workspaces** — `piclaude integrate` + conflict report.

## Claude → VSCode contract (`.pijul-carve.json`)

To drive the review UI, Claude (or anything) writes this to the repo root:

```json
{ "groups": [ { "message": "feat: X", "changeText": "<pijul diff-format text>" } ] }
```

`changeText` is a `pijul diff` payload with only that group's hunks kept (header
+ selected numbered blocks). The extension records each approved group with
`pijul record --from-change`. This is exactly what the `piclaude-carve` skill
builds — so the skill can target this file instead of recording directly, and
you validate in VSCode. The file is deleted after review (and `.ignore`d).

Caveat: groups touching **disjoint** regions are safe to batch. For two groups
editing the *same file region*, prefer *Carve* (which re-diffs between records)
— coordinate drift can make a pre-built second group fail to parse.

## How Claude fits in

Open a forked workspace, run Claude Code in the integrated terminal (or the
Claude Code extension). Claude edits files; the `piclaude-carve` skill still
works there. This extension is the **GUI counterpart**: when you'd rather pick
hunks by hand, run *Pijul: Carve*. Both paths end in `record --from-change`, so
the result is identical.

## Requirements

- `pijul` on PATH supporting `record --from-change` (set `pijul.path` otherwise).
- `piclaude` on PATH for fork/land/integrate (set `piclaude.path` otherwise).
  `land` needs a `pijul` with the `pull --no-notify` / `push --no-notify` flags.

## Build & run (dev)

```sh
cd editors/vscode
npm install
npm run compile
# then press F5 in VSCode to launch an Extension Development Host
```

## Status

POC. Compiles against the VSCode API (`npm run compile`) and the parse→record
core is verified end-to-end against the real CLI, but it is **not yet
runtime-verified** in an extension host — the command glue and SCM/QuickPick
rendering haven't been exercised live.

Per-hunk work today is *Record Hunk Under Cursor* (needs only `pijul diff`).
True **gutter change indicators** (a QuickDiffProvider) need the file's pristine
content, which pijul has no cheap command to emit yet (`archive` only makes a
tarball) — that's a small future CLI addition (`pijul cat`/file-at-channel).
