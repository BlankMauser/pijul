//! Structural fold computation via tree-sitter.
//!
//! Given the *alive* text of a file (the keep+add lines — the version
//! that actually exists, with `del`/`obs` ghosts stripped) and a map
//! from each alive row to its display-line index, this parses the file
//! and returns a nested tree of foldable regions in **display-line**
//! coordinates. The caller (the client) shows a fold only where it lies
//! entirely inside an untouched span, so hunks are never folded.
//!
//! Languages without a grammar here return `None`; the client then
//! falls back to a plain indentation heuristic.

use serde::Serialize;
use tree_sitter::{Language, Parser};

/// A foldable region, in display-line coordinates (0-based, inclusive).
#[derive(Debug, Serialize)]
pub struct FoldNode {
    pub start: u32,
    pub end: u32,
    pub children: Vec<FoldNode>,
}

/// Pick a tree-sitter grammar from a file path (by extension / basename).
pub fn language_for(path: &str) -> Option<Language> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit('.').next().unwrap_or("");
    let lang = match ext {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" | "mts" | "cts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "py" | "pyi" => tree_sitter_python::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "c" | "h" => tree_sitter_c::LANGUAGE.into(),
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" => tree_sitter_cpp::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "rb" => tree_sitter_ruby::LANGUAGE.into(),
        "json" => tree_sitter_json::LANGUAGE.into(),
        "html" | "htm" => tree_sitter_html::LANGUAGE.into(),
        "css" => tree_sitter_css::LANGUAGE.into(),
        "nix" => tree_sitter_nix::LANGUAGE.into(),
        "svelte" => tree_sitter_svelte_ng::LANGUAGE.into(),
        "ml" => tree_sitter_ocaml::LANGUAGE_OCAML.into(),
        "mli" => tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE.into(),
        "sh" | "bash" | "zsh" => tree_sitter_bash::LANGUAGE.into(),
        "hs" => tree_sitter_haskell::LANGUAGE.into(),
        _ => return None,
    };
    Some(lang)
}

/// Parse `alive_text` with `lang` and return the fold tree in display
/// coordinates. `alive_to_display[r]` is the display-line index of alive
/// row `r`. Returns an empty vec on any parse failure.
pub fn compute_folds(lang: &Language, alive_text: &str, alive_to_display: &[u32]) -> Vec<FoldNode> {
    if alive_text.is_empty() || alive_to_display.is_empty() {
        return Vec::new();
    }
    let mut parser = Parser::new();
    if parser.set_language(lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(alive_text, None) else {
        return Vec::new();
    };
    let last = alive_to_display.len() - 1;

    // Collect one foldable interval per start line — the outermost
    // (largest) multi-row named node beginning there. Expression grammars
    // (Nix, Haskell, …) nest many nodes sharing a start line into a
    // near-degenerate chain; keeping only the widest per start line yields
    // the "fold from the first line" behaviour editors use, so sub-folds
    // are only nodes that begin on a *later* line.
    let mut widest_end: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for child in node.children(&mut c) {
            stack.push(child);
            if !child.is_named() {
                continue;
            }
            let sr = child.start_position().row.min(last);
            let er = child.end_position().row.min(last);
            if er <= sr {
                continue;
            }
            let (s, e) = (alive_to_display[sr], alive_to_display[er]);
            if e <= s {
                continue;
            }
            widest_end
                .entry(s)
                .and_modify(|prev| *prev = (*prev).max(e))
                .or_insert(e);
        }
    }

    // Build the containment forest. The intervals are laminar (nested or
    // disjoint), so a start-ascending / end-descending sort plus a stack
    // reconstructs the tree.
    let mut intervals: Vec<(u32, u32)> = widest_end.into_iter().collect();
    intervals.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));

    let mut roots: Vec<FoldNode> = Vec::new();
    let mut open: Vec<FoldNode> = Vec::new();
    let attach = |open: &mut Vec<FoldNode>, roots: &mut Vec<FoldNode>, node: FoldNode| match open
        .last_mut()
    {
        Some(parent) => parent.children.push(node),
        None => roots.push(node),
    };
    for (s, e) in intervals {
        while let Some(top) = open.last() {
            if top.start <= s && e <= top.end {
                break;
            }
            let done = open.pop().unwrap();
            attach(&mut open, &mut roots, done);
        }
        open.push(FoldNode {
            start: s,
            end: e,
            children: Vec::new(),
        });
    }
    while let Some(done) = open.pop() {
        attach(&mut open, &mut roots, done);
    }
    roots
}
