// Pijul + Claude — a thin VSCode client over the pijul CLI's machine-readable
// output and the `piclaude` launcher. Same contract-first philosophy as the
// emacs modes: no shared UI code, just a translator over `pijul … --output-format
// json` + the `record --from-change` carve patch.
import * as vscode from 'vscode';
import * as cp from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

function cfg<T>(key: string, def: T): T {
  return vscode.workspace.getConfiguration().get<T>(key, def);
}
const pijulBin = () => cfg('pijul.path', 'pijul');
const piclaudeBin = () => cfg('piclaude.path', 'piclaude');

// Run a command, capturing stdout. Rejects with stderr on non-zero exit.
function run(bin: string, args: string[], cwd: string): Promise<string> {
  return new Promise((resolve, reject) => {
    cp.execFile(bin, args, { cwd, maxBuffer: 64 * 1024 * 1024 }, (err, stdout, stderr) => {
      if (err) reject(new Error(stderr || err.message));
      else resolve(stdout);
    });
  });
}

// Nearest ancestor of `start` that holds a .pijul directory.
function repoRoot(start?: string): string | undefined {
  let dir = start ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  while (dir && dir !== path.dirname(dir)) {
    if (fs.existsSync(path.join(dir, '.pijul'))) return dir;
    dir = path.dirname(dir);
  }
  return undefined;
}

// ---- the change-text format (what `pijul diff` prints) --------------------

interface Hunk { title: string; text: string; }
interface ParsedDiff { header: string; hunks: Hunk[]; }

// Split a diff into its header (through `# Hunks`) and the numbered hunk blocks.
// Rebuilding `header + a subset of blocks` yields a valid `--from-change` file.
function parseDiff(diff: string): ParsedDiff {
  const marker = '# Hunks';
  const at = diff.indexOf(marker);
  if (at < 0) return { header: diff, hunks: [] };
  const header = diff.slice(0, at) + marker + '\n\n';
  const hunks: Hunk[] = [];
  let block: string[] | null = null;
  let title = '';
  const flush = () => {
    if (block) hunks.push({ title, text: block.join('\n').replace(/\n+$/, '') + '\n' });
  };
  for (const line of diff.slice(at + marker.length).split('\n')) {
    if (/^\d+\.\s/.test(line)) { flush(); block = [line]; title = line.replace(/^\d+\.\s*/, ''); }
    else if (block) block.push(line);
  }
  flush();
  return { header, hunks };
}

function buildChange(header: string, hunks: Hunk[]): string {
  return header + hunks.map(h => h.text).join('\n');
}

// ---- SCM view: pending changes from `pijul diff --output-format json` ------

interface JsonHunk { operation: string; line: number; }

function makeScm(ctx: vscode.ExtensionContext, root: string) {
  const scm = vscode.scm.createSourceControl('pijul', 'Pijul', vscode.Uri.file(root));
  const group = scm.createResourceGroup('changes', 'Pending Changes');
  ctx.subscriptions.push(scm);

  const refresh = async () => {
    let map: Record<string, JsonHunk[]> = {};
    try {
      const out = await run(pijulBin(), ['diff', '--output-format', 'json'], root);
      map = out.trim() ? JSON.parse(out) : {};
    } catch { /* leave empty on error */ }
    group.resourceStates = Object.keys(map).sort().map(p => {
      const ops = [...new Set(map[p].map(h => h.operation))].join(', ');
      return {
        resourceUri: vscode.Uri.file(path.join(root, p)),
        decorations: { tooltip: `${ops} (${map[p].length} hunk${map[p].length > 1 ? 's' : ''})` },
        command: {
          title: 'Open',
          command: 'vscode.open',
          arguments: [vscode.Uri.file(path.join(root, p))],
        },
      };
    });
    scm.count = group.resourceStates.length;
  };
  return { refresh };
}

// ---- carve: a split-view webview to pick hunks, message them, and record ---
// Replaces the old modal QuickPick: opens beside the code, colourises the +/-
// hunks, and records the checked ones via `record --from-change`, looping in
// place (reloads the remaining hunks) so you can carve change after change.

const esc = (s: string) => s.replace(/[&<>]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]!));

// A hunk parsed for side-by-side display: old/new content lines lifted out of
// the change-text, plus context read from the working copy around the anchor.
interface Rich {
  i: number;          // index into ParsedDiff.hunks (what record --from-change needs)
  title: string;
  file: string | null;      // repo-relative path, for "select whole file"
  startLine: number | null; // 1-based working-copy line of the first `neu` line
  before: string[];   // context above (grey), from the working file
  old: string[];      // removed lines (the `-` side)
  neu: string[];      // added lines (the `+` side)
  after: string[];    // context below (grey)
}

// Locate a hunk's file + anchor line from its title. Two shapes:
//   "Replacement in "path":24 …"        → edit, anchored at working line 24
//   "File addition: "name" in "parent"" → new file, anchored at line 1
function parseTitle(title: string): { file: string | null; line: number | null } {
  let m = /\bin\s+"(.+?)":(\d+)/.exec(title);
  if (m) return { file: m[1], line: parseInt(m[2], 10) };
  m = /File addition:\s+"(.+?)"\s+in\s+"(.*?)"/.exec(title);
  if (m) return { file: m[2] ? `${m[2]}/${m[1]}` : m[1], line: 1 };
  return { file: null, line: null };
}

const CTX = 3; // lines of working-copy context shown above/below each hunk

// Parse every hunk into a Rich view: split its `-`/`+` lines (each is
// marker + one space + content) and, when we can anchor it in a real file,
// read CTX lines of surrounding context straight from the working copy — no
// pristine needed, since pijul's own diff carries both sides.
function buildViews(root: string, parsed: ParsedDiff): Rich[] {
  const cache = new Map<string, string[] | null>();
  const readLines = (rel: string): string[] | null => {
    if (!cache.has(rel)) {
      try { cache.set(rel, fs.readFileSync(path.join(root, rel), 'utf8').split('\n')); }
      catch { cache.set(rel, null); }
    }
    return cache.get(rel)!;
  };
  return parsed.hunks.map((h, i) => {
    const { file, line } = parseTitle(h.title);
    const old: string[] = [], neu: string[] = [];
    const lines = h.text.split('\n');
    for (let k = 1; k < lines.length; k++) { // k=0 is the title line
      const l = lines[k];
      const content = l[1] === ' ' ? l.slice(2) : l.slice(1);
      if (l.startsWith('+')) neu.push(content);
      else if (l.startsWith('-')) old.push(content);
    }
    let before: string[] = [], after: string[] = [];
    if (file && line) {
      const src = readLines(file);
      if (src) {
        const a = line - 1; // 0-based index of the anchor line
        before = src.slice(Math.max(0, a - CTX), a);
        after = src.slice(a + neu.length, a + neu.length + CTX);
      }
    }
    return { i, title: h.title, file, startLine: line, before, old, neu, after };
  });
}

// One <tr> of the side-by-side table: line-number gutter + cell, per side.
function sxsRow(lnL: number | null, textL: string | null, clsL: string,
               lnR: number | null, textR: string | null, clsR: string): string {
  const side = (n: number | null, t: string | null, cls: string) =>
    `<td class="ln">${n ?? ''}</td><td class="cell ${t === null ? '' : cls}">${t === null ? '' : (esc(t) || '&nbsp;')}</td>`;
  return `<tr>${side(lnL, textL, clsL)}${side(lnR, textR, clsR)}</tr>`;
}

// Render a hunk as a real two-column diff: grey context, red `old` on the
// left, green `neu` on the right, working-copy line numbers on both gutters.
function sideBySide(h: Rich): string {
  if (!h.before.length && !h.old.length && !h.neu.length && !h.after.length) return '';
  const rows: string[] = [];
  const base = h.startLine ?? 1;
  h.before.forEach((t, k) => { const n = base - h.before.length + k; rows.push(sxsRow(n, t, 'ctx', n, t, 'ctx')); });
  let rn = base;
  const m = Math.max(h.old.length, h.neu.length);
  for (let k = 0; k < m; k++) {
    const l = k < h.old.length ? h.old[k] : null;
    const r = k < h.neu.length ? h.neu[k] : null;
    rows.push(sxsRow(null, l, 'del', r === null ? null : rn++, r, 'add'));
  }
  const afterStart = base + h.neu.length;
  h.after.forEach((t, k) => { const n = afterStart + k; rows.push(sxsRow(n, t, 'ctx', n, t, 'ctx')); });
  return `<table class="sxs">${rows.join('')}</table>`;
}

// Lock/generated files: rarely reviewed line-by-line, so fold them regardless
// of size. Source files only fold once they get genuinely long.
const LOCKFILE = /(^|\/)(package-lock\.json|npm-shrinkwrap\.json|yarn\.lock|pnpm-lock\.yaml|Cargo\.lock|Cargo\.nix|flake\.lock|poetry\.lock|Gemfile\.lock|composer\.lock|go\.sum|Pipfile\.lock)$/i;
const GENERATED = /\.(min\.(js|css)|map)$/i;
const SOURCE_COLLAPSE = 40; // fold a source hunk past this many changed lines

// Decide whether a hunk starts folded, and why (shown in the summary).
function foldDecision(h: Rich): { fold: boolean; reason: string } {
  if (h.file && (LOCKFILE.test(h.file) || GENERATED.test(h.file)))
    return { fold: true, reason: 'lock / generated file' };
  const size = h.old.length + h.neu.length;
  if (size > SOURCE_COLLAPSE) return { fold: true, reason: `${size} lines` };
  return { fold: false, reason: '' };
}

function carveHtml(views: Rich[], nonce: string): string {
  const rows = views.map(h => {
    const table = sideBySide(h);
    const { fold, reason } = foldDecision(h);
    const body = fold
      ? `<details><summary>Show diff · +${h.neu.length} −${h.old.length} · ${reason}</summary>${table}</details>`
      : table;
    const fileAttr = h.file === null ? '' : ` data-file="${esc(h.file).replace(/"/g, '&quot;')}"`;
    return `
    <div class="hunk">
      <label class="hrow"><input type="checkbox" data-i="${h.i}"${fileAttr}><span class="title">${esc(h.title)}</span></label>
      ${body}
    </div>`;
  }).join('');
  return `<!DOCTYPE html><html><head><meta charset="utf-8">
  <meta http-equiv="Content-Security-Policy"
    content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';">
  <style>
    body { font-family: var(--vscode-font-family); color: var(--vscode-foreground); padding: 0 8px 80px; }
    .bar { position: sticky; top: 0; background: var(--vscode-editor-background); padding: 8px 0;
           display: flex; gap: 6px; align-items: center; flex-wrap: wrap;
           border-bottom: 1px solid var(--vscode-panel-border); z-index: 1; }
    input[type=text] { flex: 1; min-width: 180px; padding: 4px 6px;
      background: var(--vscode-input-background); color: var(--vscode-input-foreground);
      border: 1px solid var(--vscode-input-border, transparent); }
    button { padding: 4px 12px; cursor: pointer;
      background: var(--vscode-button-background); color: var(--vscode-button-foreground); border: none; }
    button.secondary { background: var(--vscode-button-secondaryBackground); color: var(--vscode-button-secondaryForeground); }
    button:disabled { opacity: .5; cursor: default; }
    .hunk { padding: 8px 4px; border-bottom: 1px solid var(--vscode-panel-border); scroll-margin-top: 56px; }
    .hunk.focused { background: var(--vscode-list-inactiveSelectionBackground); outline: 1px solid var(--vscode-focusBorder); }
    .hint { color: var(--vscode-descriptionForeground); font-size: 11px; padding: 4px 0 2px; }
    .hrow { display: flex; gap: 8px; align-items: center; margin-bottom: 6px; cursor: pointer; }
    .title { font-weight: 600; }
    details > summary { cursor: pointer; color: var(--vscode-textLink-foreground); padding: 2px 0; user-select: none; }
    details[open] > summary { margin-bottom: 4px; }
    .sxs { width: 100%; border-collapse: collapse; table-layout: fixed;
           font-family: var(--vscode-editor-font-family); font-size: var(--vscode-editor-font-size); }
    .sxs td.ln { width: 3.4em; text-align: right; padding: 0 6px 0 2px; vertical-align: top;
                 color: var(--vscode-editorLineNumber-foreground); user-select: none; }
    .sxs td.cell { width: calc(50% - 3.6em); vertical-align: top; padding: 0 6px;
                   white-space: pre-wrap; word-break: break-all; }
    .cell.add { background: var(--vscode-diffEditor-insertedTextBackground, rgba(78,201,78,.15)); }
    .cell.del { background: var(--vscode-diffEditor-removedTextBackground, rgba(209,105,105,.15)); }
    .cell.ctx { color: var(--vscode-descriptionForeground); }
    .empty { padding: 24px 4px; color: var(--vscode-descriptionForeground); }
    #status { color: var(--vscode-descriptionForeground); }
  </style></head><body>
    <div class="bar">
      <button class="secondary" id="all">All</button>
      <button class="secondary" id="none">None</button>
      <input type="text" id="msg" placeholder="Change message…">
      <button id="rec" disabled>Record selected</button>
      <button class="secondary" id="refresh">↻</button>
      <span id="status"></span>
    </div>
    <div class="hint">j/k move · space toggle · shift+j/k or shift-click: range · f: whole file · a/n: all/none · g/G: ends · m: message · enter: record</div>
    ${views.length ? rows : '<div class="empty">Nothing to record — working copy is clean.</div>'}
    <script nonce="${nonce}">
      const vscode = acquireVsCodeApi();
      const msg = document.getElementById('msg'), rec = document.getElementById('rec'), status = document.getElementById('status');
      const bs = () => [...document.querySelectorAll('input[type=checkbox]')];
      const hs = () => [...document.querySelectorAll('.hunk')];
      const sel = () => bs().filter(b => b.checked).map(b => +b.dataset.i);
      const sync = () => { const n = sel().length; rec.disabled = n === 0 || !msg.value.trim();
        rec.textContent = n ? \`Record \${n} hunk\` + (n > 1 ? 's' : '') : 'Record selected'; };

      // focus = keyboard cursor over hunks; anchor = start of a range selection.
      let focus = 0, anchor = 0;
      const paint = () => hs().forEach((h, i) => h.classList.toggle('focused', i === focus));
      const setFocus = (i) => { const n = bs().length; if (!n) return;
        focus = Math.max(0, Math.min(n - 1, i)); paint(); hs()[focus].scrollIntoView({ block: 'nearest' }); };
      const setAll = (v) => { bs().forEach(b => b.checked = v); sync(); };
      const rangeTo = (i, v) => { const a = Math.min(anchor, i), z = Math.max(anchor, i);
        bs().forEach((b, j) => { if (j >= a && j <= z) b.checked = v; }); sync(); };
      const selectFile = (i) => { const f = bs()[i] && bs()[i].dataset.file; if (f == null) return;
        bs().forEach(b => { if (b.dataset.file === f) b.checked = true; }); sync(); };

      document.getElementById('all').onclick = () => setAll(true);
      document.getElementById('none').onclick = () => setAll(false);
      document.getElementById('refresh').onclick = () => vscode.postMessage({ type: 'refresh' });
      msg.oninput = sync; document.addEventListener('change', sync);
      rec.onclick = () => { rec.disabled = true; status.textContent = 'Recording…';
        vscode.postMessage({ type: 'record', indices: sel(), message: msg.value.trim() }); };
      window.addEventListener('message', e => { if (e.data.type === 'status') status.textContent = e.data.text; });

      // pointer: focus the clicked hunk; shift-click a box paints the range.
      document.addEventListener('click', e => {
        const hunk = e.target.closest('.hunk'); if (hunk) { focus = hs().indexOf(hunk); paint(); }
        const b = e.target.closest('input[type=checkbox]'); if (!b) return;
        const i = bs().indexOf(b);
        if (e.shiftKey) rangeTo(i, b.checked); else anchor = i;
      });

      document.addEventListener('keydown', e => {
        if (document.activeElement === msg) {
          if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); rec.click(); }
          else if (e.key === 'Escape') msg.blur();
          return;
        }
        const move = (d) => { if (e.shiftKey) { setFocus(focus + d); rangeTo(focus, bs()[anchor].checked); } else setFocus(focus + d); };
        switch (e.key) {
          case 'j': case 'ArrowDown': e.preventDefault(); move(1); break;
          case 'k': case 'ArrowUp': e.preventDefault(); move(-1); break;
          case ' ': case 'x': e.preventDefault(); { const b = bs()[focus]; if (b) { b.checked = !b.checked; anchor = focus; sync(); } } break;
          case 'a': e.preventDefault(); setAll(true); break;
          case 'n': e.preventDefault(); setAll(false); break;
          case 'f': e.preventDefault(); selectFile(focus); break;
          case 'g': e.preventDefault(); setFocus(0); break;
          case 'G': e.preventDefault(); setFocus(bs().length - 1); break;
          case 'm': case '/': e.preventDefault(); msg.focus(); break;
          case 'Enter': e.preventDefault(); if (!rec.disabled) rec.click(); else msg.focus(); break;
        }
      });
      setFocus(0);
    </script>
  </body></html>`;
}

function carvePanel(ctx: vscode.ExtensionContext, root: string, refresh: () => Promise<void>) {
  const panel = vscode.window.createWebviewPanel(
    'pijulCarve', 'Pijul: Carve', vscode.ViewColumn.Beside,
    { enableScripts: true, retainContextWhenHidden: true });
  let state: ParsedDiff = { header: '', hunks: [] };
  let nonceN = 0;

  const load = async () => {
    try {
      state = parseDiff(await run(pijulBin(), ['diff'], root));
    } catch (e: any) {
      vscode.window.showErrorMessage(`Pijul diff failed: ${e.message}`);
      state = { header: '', hunks: [] };
    }
    panel.webview.html = carveHtml(buildViews(root, state), `n${nonceN++}${process.pid}`);
  };

  panel.webview.onDidReceiveMessage(async (m: any) => {
    if (m.type === 'refresh') { await load(); return; }
    if (m.type !== 'record') return;
    const picked = (m.indices as number[]).map(i => state.hunks[i]).filter(Boolean);
    if (!picked.length || !m.message) return;
    const file = path.join(os.tmpdir(), `pijul-carve-${process.pid}-${nonceN}.change`);
    fs.writeFileSync(file, buildChange(state.header, picked));
    try {
      await run(pijulBin(), ['record', '--from-change', file, '-m', m.message], root);
    } catch (e: any) {
      panel.webview.postMessage({ type: 'status', text: `❌ ${e.message}` });
      vscode.window.showErrorMessage(`Pijul record failed: ${e.message}`);
      return;
    } finally {
      fs.rmSync(file, { force: true });
    }
    await refresh();
    await load(); // reload with the remaining hunks
    panel.webview.postMessage({ type: 'status', text: `✔ recorded “${m.message}”` });
  }, undefined, ctx.subscriptions);

  load();
  return panel;
}

// ---- #1: record the single hunk under the cursor --------------------------

async function recordHunkAtCursor(root: string, refresh: () => Promise<void>) {
  const ed = vscode.window.activeTextEditor;
  if (!ed) { vscode.window.showErrorMessage('Pijul: no active editor.'); return; }
  const rel = path.relative(root, ed.document.uri.fsPath);
  const cursor = ed.selection.active.line + 1; // 1-based, matches hunk titles
  const { header, hunks } = parseDiff(await run(pijulBin(), ['diff'], root));
  const inFile = hunks
    .map(h => ({ h, m: /in "(.+?)":(\d+)/.exec(h.title) }))
    .filter(x => x.m && x.m[1] === rel)
    .map(x => ({ h: x.h, line: parseInt(x.m![2], 10) }))
    .sort((a, b) => a.line - b.line);
  if (inFile.length === 0) { vscode.window.showInformationMessage(`Pijul: no pending hunk in ${rel}.`); return; }
  let chosen = inFile[0];
  for (const c of inFile) if (c.line <= cursor) chosen = c; // nearest at/above cursor
  const op = chosen.h.title.split(' ')[0].toLowerCase();
  const message = await vscode.window.showInputBox({
    title: 'Record hunk under cursor', prompt: 'Change message', value: `${op} ${rel}`,
    validateInput: v => (v.trim() ? undefined : 'A message is required'),
  });
  if (!message) return;
  const file = path.join(os.tmpdir(), `pijul-hunk-${process.pid}.change`);
  fs.writeFileSync(file, buildChange(header, [chosen.h]));
  try {
    await run(pijulBin(), ['record', '--from-change', file, '-m', message], root);
  } catch (e: any) {
    vscode.window.showErrorMessage(`Pijul record failed: ${e.message}`); return;
  } finally {
    fs.rmSync(file, { force: true });
  }
  await refresh();
  vscode.window.showInformationMessage(`Recorded hunk: ${message}`);
}

// ---- #2: review a decomposition Claude proposed (file-based handoff) -------

interface GroupItem extends vscode.QuickPickItem {
  group: { message: string; changeText: string };
  index: number;
}

// One-line summary of a change-text for the QuickPick detail.
function summarizeChange(text: string): string {
  const { hunks } = parseDiff(text);
  const files = [...new Set(hunks.map(h => /in "(.+?)"/.exec(h.title)?.[1] ?? '?'))];
  return `${hunks.length} hunk(s) · ${files.join(', ')}`;
}

async function previewChange(text: string) {
  const doc = await vscode.workspace.openTextDocument({ content: text, language: 'diff' });
  vscode.window.showTextDocument(doc, { preview: true, preserveFocus: true });
}

// Contract: Claude writes {groups:[{message, changeText}]} (changeText in
// `pijul diff` format) to the watched proposal file. We render it for approval
// and record the approved groups via `record --from-change`. Same gesture as
// the piclaude-carve skill, but the human validates in VSCode.
async function carveGroups(root: string, refresh: () => Promise<void>, proposalPath: string) {
  let groups: { message: string; changeText: string }[];
  try {
    groups = JSON.parse(fs.readFileSync(proposalPath, 'utf8')).groups;
  } catch (e: any) {
    vscode.window.showErrorMessage(`Cannot read carve proposal: ${e.message}`); return;
  }
  if (!groups?.length) { vscode.window.showInformationMessage('Pijul: empty carve proposal.'); return; }

  const items: GroupItem[] = groups.map((g, i) => ({
    label: g.message, detail: summarizeChange(g.changeText), picked: true, group: g, index: i,
    buttons: [{ iconPath: new vscode.ThemeIcon('eye'), tooltip: 'Preview change' }],
  }));
  const qp = vscode.window.createQuickPick<GroupItem>();
  qp.title = `Claude proposed ${groups.length} change(s) — approve which to record`;
  qp.canSelectMany = true;
  qp.items = items;
  qp.selectedItems = items;
  qp.onDidTriggerItemButton(e => previewChange(e.item.group.changeText));
  const chosen = await new Promise<readonly GroupItem[]>(resolve => {
    qp.onDidAccept(() => { resolve(qp.selectedItems); qp.hide(); });
    qp.onDidHide(() => resolve(qp.selectedItems));
    qp.show();
  });
  qp.dispose();
  if (!chosen.length) return;

  for (const it of chosen) {
    const file = path.join(os.tmpdir(), `pijul-group-${process.pid}-${it.index}.change`);
    const text = it.group.changeText.endsWith('\n') ? it.group.changeText : it.group.changeText + '\n';
    fs.writeFileSync(file, text);
    try {
      await run(pijulBin(), ['record', '--from-change', file, '-m', it.group.message], root);
    } catch (e: any) {
      vscode.window.showErrorMessage(`Record "${it.group.message}" failed: ${e.message}`); break;
    } finally {
      fs.rmSync(file, { force: true });
    }
  }
  fs.rmSync(proposalPath, { force: true });
  await refresh();
  vscode.window.showInformationMessage(`Recorded ${chosen.length} change(s).`);
}

// ---- land: the only write path to main ------------------------------------
// All the logic (record in the fork, then under a lock pull main into the fork
// and push back, stopping on conflict) lives in `piclaude land`. We just drive
// it and surface the two outcomes that matter: landed, or stopped on a conflict
// to resolve right here in the fork. Contract-first, like fork/integrate.

async function land(root: string, refresh: () => Promise<void>) {
  const mode = await vscode.window.showQuickPick(
    [
      { label: '$(cloud-upload) Record & land', amend: false },
      { label: '$(pencil) Amend last change & land', amend: true },
    ],
    { title: 'Pijul: land to main', placeHolder: 'How should this land be recorded?' },
  );
  if (!mode) return;
  const message = await vscode.window.showInputBox({
    title: mode.amend ? 'Amend last change & land' : 'Record & land',
    prompt: mode.amend ? 'New message (blank = keep the existing one)' : 'Change message',
    validateInput: v => (mode.amend || v.trim() ? undefined : 'A message is required'),
  });
  if (message === undefined) return; // cancelled

  const args = ['land',
    ...(mode.amend ? ['--amend'] : []),
    ...(message.trim() ? ['-m', message.trim()] : [])];
  try {
    await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: 'Pijul: landing to main…' },
      () => run(piclaudeBin(), args, root),
    );
  } catch (e: any) {
    await refresh(); // the pull may have moved files even on the conflict path
    const msg = e.message || '';
    if (/conflict|conflit/i.test(msg)) {
      // piclaude land pulled main in, hit a conflict, released the lock and left
      // markers in the fork. Resolve them here, then land again (it's re-runnable).
      vscode.window.showWarningMessage(
        'Pijul land: conflict pulling main into your fork. Resolve the ' +
        '<<<<<<< / >>>>>>> markers in the changed files, then run “Pijul: Land to Main” ' +
        'again — it will record your resolution and push.');
    } else {
      vscode.window.showErrorMessage(`piclaude land failed: ${msg}`);
    }
    return;
  }
  await refresh();
  vscode.window.showInformationMessage('Pijul: landed on main.');
}

// ---- activation -----------------------------------------------------------

export function activate(ctx: vscode.ExtensionContext) {
  const root = repoRoot();
  const scm = root ? makeScm(ctx, root) : undefined;
  if (scm) scm.refresh();

  const needRoot = (): string | undefined => {
    const r = repoRoot();
    if (!r) vscode.window.showErrorMessage('Pijul: no .pijul repository in this workspace.');
    return r;
  };

  const doRefresh = async () => { await scm?.refresh(); };
  const proposalRel = cfg('pijul.carveProposal', '.pijul-carve.json');

  ctx.subscriptions.push(
    vscode.commands.registerCommand('pijul.refresh', () => scm?.refresh()),

    vscode.commands.registerCommand('pijul.carve', async () => {
      const r = needRoot(); if (!r) return;
      carvePanel(ctx, r, doRefresh);
    }),

    vscode.commands.registerCommand('pijul.recordHunk', async () => {
      const r = needRoot(); if (!r) return;
      await recordHunkAtCursor(r, doRefresh);
    }),

    vscode.commands.registerCommand('pijul.carveGroups', async () => {
      const r = needRoot(); if (!r) return;
      await carveGroups(r, doRefresh, path.join(r, proposalRel));
    }),

    vscode.commands.registerCommand('pijul.fork', async () => {
      const r = needRoot(); if (!r) return;
      const name = await vscode.window.showInputBox({ title: 'New agent workspace', prompt: 'Name (blank = auto)' });
      if (name === undefined) return;
      try {
        const out = await run(piclaudeBin(), ['fork', ...(name ? [name] : [])], r);
        const dest = out.trim().split('\n').pop()!;
        const open = await vscode.window.showInformationMessage(`Forked ${dest}`, 'Open in New Window');
        if (open) vscode.commands.executeCommand('vscode.openFolder', vscode.Uri.file(dest), true);
      } catch (e: any) {
        vscode.window.showErrorMessage(`piclaude fork failed: ${e.message}`);
      }
    }),

    vscode.commands.registerCommand('pijul.land', async () => {
      const r = needRoot(); if (!r) return;
      await land(r, doRefresh);
    }),

    vscode.commands.registerCommand('pijul.integrate', async () => {
      const r = needRoot(); if (!r) return;
      try {
        const out = await run(piclaudeBin(), ['integrate'], r);
        await scm?.refresh();
        vscode.window.showInformationMessage(`Pijul integrate:\n${out.trim() || 'done'}`);
      } catch (e: any) {
        vscode.window.showErrorMessage(`piclaude integrate failed: ${e.message}`);
      }
    }),
  );

  // Claude → VSCode handoff: when the proposal file appears, pop the review UI.
  if (root) {
    const watcher = vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(root, proposalRel));
    const trigger = () => carveGroups(root, doRefresh, path.join(root, proposalRel));
    watcher.onDidCreate(trigger);
    watcher.onDidChange(trigger);
    ctx.subscriptions.push(watcher);
  }
}

export function deactivate() {}
