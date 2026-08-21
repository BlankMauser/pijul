use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use canonical_path::CanonicalPathBuf;
use clap::Parser;
use pijul_core::change::*;
use pijul_core::working_copy::WorkingCopyRead;
use pijul_core::{MutTxnT, TxnTExt};
use serde_derive::Serialize;

use crate::commands::common_opts::{DiffAlgorithm, OutputFormat, RepoAndChannel};

use pijul_repository::*;

#[derive(Parser, Debug)]
pub struct Diff {
    #[clap(flatten)]
    pub base: RepoAndChannel,
    /// Output format. `json` emits a structured diff instead of the default
    /// human-readable change text.
    #[clap(long = "output-format", value_enum)]
    pub output_format: Option<OutputFormat>,
    /// Deprecated alias for `--output-format json`.
    #[clap(long = "json", hide = true)]
    pub json: bool,
    /// Show the uncommitted change inlined in the full context of each file
    /// (a structured JSON "palimpsest": added / removed spans over the whole
    /// file). This is the record-time preview; intended for editor clients.
    #[clap(long = "context")]
    pub context: bool,
    /// Show a short version of the diff.
    #[clap(short = 's', long = "short")]
    pub short: bool,
    /// Include the untracked files
    #[clap(short = 'u', long = "untracked")]
    pub untracked: bool,
    /// Only diff those paths (files or directories). If missing, diff the entire repository.
    pub prefixes: Vec<PathBuf>,
    /// Diff algorithm to use (default: myers)
    #[clap(long = "algorithm", value_enum)]
    pub algorithm: Option<DiffAlgorithm>,
}

impl Diff {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    /// Whether JSON output was requested, via either `--output-format json`
    /// or the deprecated `--json` alias.
    fn is_json(&self) -> bool {
        self.json || matches!(self.output_format, Some(OutputFormat::Json))
    }

    pub fn run(mut self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let repo = self.base.find_root()?;
        let txn = repo.pristine.arc_txn_begin()?;
        let mut stdout = std::io::stdout();

        if self.untracked && self.is_json() {
            serde_json::to_writer_pretty(
                &mut std::io::stdout(),
                &untracked(&repo, txn.clone())?.collect::<Result<Vec<_>, _>>()?,
            )?;
            writeln!(stdout)?;
            return Ok(());
        }

        let channel_name = get_channel(self.base.channel(), &*txn.read()).0.to_string();
        let channel_sm: pijul_core::small_string::SmallString = channel_name.parse()?;
        let channel = txn.write().open_or_create_channel(&channel_sm)?;

        let mut state = pijul_core::RecordBuilder::new();
        let algorithm = match self.algorithm {
            Some(DiffAlgorithm::Patience) => pijul_core::Algorithm::Patience,
            Some(DiffAlgorithm::Histogram) => pijul_core::Algorithm::ImaraHistogram,
            None => pijul_core::Algorithm::default(),
        };
        if self.prefixes.is_empty() {
            state.record(
                txn.clone(),
                algorithm,
                self.short,
                &pijul_core::DEFAULT_SEPARATOR,
                channel.clone(),
                &repo.working_copy,
                &repo.changes,
                "",
                std::thread::available_parallelism()?.get(),
            )?
        } else {
            self.fill_relative_prefixes()?;
            repo.working_copy.record_prefixes(
                txn.clone(),
                algorithm,
                channel.clone(),
                &repo.changes,
                &mut state,
                CanonicalPathBuf::canonicalize(&repo.path)?,
                &self.prefixes,
                false,
                std::thread::available_parallelism()?.get(),
                0,
            )?;
        }
        let rec = state.finish();
        // `diff` runs the same walk as `record` but never applies anything, so it
        // re-diffs every file the stat cache couldn't already prove clean. Warm
        // the cache from the files this walk confirmed clean (content matched the
        // pristine) so the next `diff`/`record` skips them. Only the clean ones —
        // a file that actually differs must never be cached as clean, or the
        // pending edit would be hidden. See notes-record-stat-cache.md.
        let stat_updates = rec.take_stat_updates();

        // The record-time palimpsest preview: group the recorded hunks by
        // file and annotate each file's working-copy content in full context.
        if self.context {
            let mut by_path: BTreeMap<String, Vec<PreviewHunk>> = BTreeMap::new();
            for a in rec.actions.iter() {
                by_path
                    .entry(preview_hunk_path(a).to_string())
                    .or_default()
                    .push(a.clone());
            }
            let txnr = txn.read();
            let mut files = Vec::new();
            for (path, actions) in by_path {
                // Working-copy ("new") content; a deleted file simply has none.
                let mut buf = Vec::new();
                let _ = repo.working_copy.read_file(&path, &mut buf);
                let content = String::from_utf8_lossy(&buf).into_owned();
                let segments =
                    palimpsest::record_preview_actions(&*txnr, &repo.changes, &actions, &content)?;
                if !segments.is_empty() {
                    files.push(PreviewFile { path, segments });
                }
            }
            drop(txnr);
            serde_json::to_writer_pretty(&mut stdout, &PreviewResponse { files })?;
            writeln!(stdout)?;
            return Ok(());
        }

        if rec.actions.is_empty() {
            if self.short && self.untracked {
                print_untracked_files(&repo, txn.clone())?;
            } else if self.untracked {
                for path in untracked(&repo, txn.clone())? {
                    let path = path?;
                    writeln!(
                        stdout,
                        "{}",
                        path.to_str()
                            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?
                    )?;
                }
            }
            // Whole tree matches the pristine: warm the cache with every file
            // this walk confirmed clean so the next diff/record skips them.
            {
                let mut txn_ = txn.write();
                pijul_core::record::update_stat_cache(&mut *txn_, &stat_updates, true)
                    .map_err(|e| anyhow::anyhow!("stat cache: {:?}", e))?;
            }
            txn.commit()?;
            return Ok(());
        }
        let actions: Vec<_> = {
            let txn_ = txn.read();
            rec.actions
                .into_iter()
                .map(|rec| rec.globalize(&*txn_).unwrap())
                .collect()
        };
        let actions_is_empty = actions.is_empty();
        let contents = if let Ok(cont) = std::sync::Arc::try_unwrap(rec.contents) {
            cont.into_inner()
        } else {
            unreachable!()
        };
        let mut change = LocalChange::make_change(
            &*txn.read(),
            &channel,
            actions,
            contents,
            ChangeHeader::default(),
            Vec::new(),
        )?;

        let (dependencies, extra_known) = {
            let txn_ = txn.read();
            dependencies(&*txn_, &*channel.read(), change.changes.iter())?
        };
        change.dependencies = dependencies;
        change.extra_known = extra_known;

        let colors = is_colored(config);
        if self.is_json() {
            let mut changes = BTreeMap::new();
            for ch in change.changes.iter() {
                changes
                    .entry(ch.path())
                    .or_insert_with(Vec::new)
                    .push(Status {
                        operation: match ch {
                            Hunk::FileMove { .. } => "file move",
                            Hunk::FileDel { .. } => "file del",
                            Hunk::FileUndel { .. } => "file undel",
                            Hunk::SolveNameConflict { .. } => "solve name conflict",
                            Hunk::UnsolveNameConflict { .. } => "unsolve name conflict",
                            Hunk::FileAdd { .. } => "file add",
                            Hunk::Edit { .. } => "edit",
                            Hunk::Replacement { .. } => "replacement",
                            Hunk::SolveOrderConflict { .. } => "solve order conflict",
                            Hunk::UnsolveOrderConflict { .. } => "unsolve order conflict",
                            Hunk::ResurrectZombies { .. } => "resurrect zombies",
                            Hunk::AddRoot { .. } => "root",
                            Hunk::DelRoot { .. } => "unroot",
                        },
                        line: ch.line(),
                    });
            }
            serde_json::to_writer_pretty(&mut std::io::stdout(), &changes)?;
            writeln!(stdout)?;
        } else if self.short {
            let mut changes = BTreeMap::new();
            for ch in change.changes.iter() {
                match ch {
                    Hunk::FileMove { path, .. } => {
                        changes.entry(path).or_insert(BTreeSet::new()).insert("MV")
                    }
                    Hunk::FileDel { path, .. } => {
                        changes.entry(path).or_insert(BTreeSet::new()).insert("D")
                    }
                    Hunk::FileUndel { path, .. } => {
                        changes.entry(path).or_insert(BTreeSet::new()).insert("UD")
                    }
                    Hunk::FileAdd { path, .. } => {
                        changes.entry(path).or_insert(BTreeSet::new()).insert("A")
                    }
                    Hunk::SolveNameConflict { path, .. } => {
                        changes.entry(path).or_insert(BTreeSet::new()).insert("SC")
                    }
                    Hunk::UnsolveNameConflict { path, .. } => {
                        changes.entry(path).or_insert(BTreeSet::new()).insert("UC")
                    }
                    Hunk::Edit {
                        local: Local { path, .. },
                        ..
                    } => changes.entry(path).or_insert(BTreeSet::new()).insert("M"),
                    Hunk::Replacement {
                        local: Local { path, .. },
                        ..
                    } => changes.entry(path).or_insert(BTreeSet::new()).insert("R"),
                    Hunk::SolveOrderConflict {
                        local: Local { path, .. },
                        ..
                    } => changes.entry(path).or_insert(BTreeSet::new()).insert("SC"),
                    Hunk::UnsolveOrderConflict {
                        local: Local { path, .. },
                        ..
                    } => changes.entry(path).or_insert(BTreeSet::new()).insert("UC"),
                    Hunk::ResurrectZombies {
                        local: Local { path, .. },
                        ..
                    } => changes.entry(path).or_insert(BTreeSet::new()).insert("RZ"),
                    Hunk::AddRoot { .. } | Hunk::DelRoot { .. } => true,
                };
            }
            let al = changes
                .iter()
                .map(|(_, v)| v.iter().map(|x| x.len()).sum::<usize>() + v.len() - 1)
                .max()
                .unwrap_or(0);
            let spaces: String = std::iter::repeat(' ').take(al).collect();
            for (k, v) in changes.iter() {
                let mut is_first = true;
                for v in v.iter() {
                    if is_first {
                        write!(stdout, "{}", v)?;
                    } else {
                        write!(stdout, ",{}", v)?;
                    }
                    is_first = false;
                }
                let (sp, _) = spaces.split_at(al - v.len());
                writeln!(stdout, "{} {}", sp, k)?;
            }
            if self.untracked {
                print_untracked_files(&repo, txn.clone())?;
            }
        } else if self.untracked {
            for path in untracked(&repo, txn.clone())? {
                writeln!(stdout, "{}", path?.to_str().unwrap())?;
            }
        } else {
            match change.write(
                &repo.changes,
                None,
                true,
                Colored {
                    w: termcolor::StandardStream::stdout(termcolor::ColorChoice::Auto),
                    colors,
                },
            ) {
                Ok(()) => {}
                Err(pijul_core::change::TextSerError::Io(e))
                    if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                Err(e) => return Err(e.into()),
            }
        }
        // Persist the stat cache warmed by this walk (clean-only, so a pending
        // edit is never cached as clean) and commit, so the next diff/record
        // skips these files instead of re-diffing the whole tree.
        {
            let mut txn_ = txn.write();
            pijul_core::record::update_stat_cache(&mut *txn_, &stat_updates, true)
                .map_err(|e| anyhow::anyhow!("stat cache: {:?}", e))?;
            if actions_is_empty && self.prefixes.is_empty() {
                use pijul_core::ChannelMutTxnT;
                txn_.touch_channel(&mut *channel.write(), None);
            }
        }
        txn.commit()?;
        Ok(())
    }

    fn fill_relative_prefixes(&mut self) -> Result<(), anyhow::Error> {
        let cwd = std::env::current_dir()?;
        for p in self.prefixes.iter_mut() {
            if p.is_relative() {
                *p = cwd.join(&p);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct Status {
    operation: &'static str,
    line: Option<usize>,
}

/// A recorded (uncommitted) hunk, as produced by `RecordBuilder::finish`.
type PreviewHunk =
    pijul_core::change::Hunk<Option<pijul_core::pristine::ChangeId>, pijul_core::change::LocalByte>;

/// The `diff --context` payload: the record-time palimpsest preview, one entry
/// per file the uncommitted change touches.
#[derive(Serialize)]
struct PreviewResponse {
    files: Vec<PreviewFile>,
}

#[derive(Serialize)]
struct PreviewFile {
    path: String,
    segments: Vec<palimpsest::Segment>,
}

/// The path a recorded hunk touches. (`Hunk::path` is only implemented for the
/// `Local` variant, not the `LocalByte` one that `Recorded` carries.)
fn preview_hunk_path(h: &PreviewHunk) -> &str {
    use pijul_core::change::Hunk;
    match h {
        Hunk::FileMove { path, .. }
        | Hunk::FileDel { path, .. }
        | Hunk::FileUndel { path, .. }
        | Hunk::SolveNameConflict { path, .. }
        | Hunk::UnsolveNameConflict { path, .. }
        | Hunk::FileAdd { path, .. } => path,
        Hunk::Edit { local, .. }
        | Hunk::Replacement { local, .. }
        | Hunk::SolveOrderConflict { local, .. }
        | Hunk::UnsolveOrderConflict { local, .. }
        | Hunk::ResurrectZombies { local, .. } => &local.path,
        Hunk::AddRoot { .. } | Hunk::DelRoot { .. } => "/",
    }
}

pub struct Colored<W> {
    pub w: W,
    pub colors: bool,
}

impl<W: std::io::Write> std::io::Write for Colored<W> {
    fn write(&mut self, s: &[u8]) -> Result<usize, std::io::Error> {
        self.w.write(s)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.w.flush()
    }
}

use crate::commands::get_channel;
use termcolor::*;

impl<W: termcolor::WriteColor> pijul_core::change::WriteChangeLine for Colored<W> {
    fn write_change_line(&mut self, pref: &str, contents: &str) -> Result<(), std::io::Error> {
        if self.colors {
            let col = if pref == "+" {
                Color::Green
            } else {
                Color::Red
            };
            self.w.set_color(ColorSpec::new().set_fg(Some(col)))?;
            writeln!(self.w, "{} {}", pref, contents)?;
            self.w.reset()
        } else {
            writeln!(self.w, "{} {}", pref, contents)
        }
    }
    fn write_change_line_binary(
        &mut self,
        pref: &str,
        contents: &[u8],
    ) -> Result<(), std::io::Error> {
        if self.colors {
            let col = if pref == "+" {
                Color::Green
            } else {
                Color::Red
            };
            self.w.set_color(ColorSpec::new().set_fg(Some(col)))?;
            write!(
                self.w,
                "{}b{}",
                pref,
                data_encoding::BASE64.encode(contents)
            )?;
            self.w.reset()
        } else {
            write!(
                self.w,
                "{}b{}",
                pref,
                data_encoding::BASE64.encode(contents)
            )
        }
    }
}

pub fn is_colored(config: &pijul_config::Config) -> bool {
    let mut colors = std::io::stdout().is_terminal();
    match config.colors {
        pijul_config::Choice::Auto => (),
        pijul_config::Choice::Always => colors = true,
        pijul_config::Choice::Never => colors = false,
    }
    match config.pager {
        pijul_config::Choice::Never => colors = false,
        _ => {
            super::pager(config);
        }
    }
    colors
}

pub fn print_untracked_files<T: TxnTExt + Send + Sync + 'static>(
    repo: &Repository,
    txn: pijul_core::ArcTxn<T>,
) -> Result<(), anyhow::Error> {
    let mut stdout = std::io::stdout();
    for path in untracked(&repo, txn)? {
        writeln!(stdout, "U {}", path?.to_str().unwrap())?;
    }
    Ok(())
}

pub(super) fn untracked<T: TxnTExt + Send + Sync + 'static>(
    repo: &Repository,
    txn: pijul_core::ArcTxn<T>,
) -> Result<impl Iterator<Item = Result<PathBuf, std::io::Error>>, anyhow::Error> {
    let repo_path = CanonicalPathBuf::canonicalize(&repo.path)?;
    let threads = std::thread::available_parallelism()?.get();
    let txn_ = txn.clone();
    Ok(repo
        .working_copy
        .iterate_prefix_rec(
            repo_path.clone(),
            repo_path.clone(),
            false,
            threads,
            move |path, _| {
                use path_slash::PathExt;
                let path_str = path.to_slash_lossy();
                log::debug!("untracked {:?}", path_str);
                path_str.is_empty() || txn.read().is_tracked(&path_str).unwrap()
            },
        )?
        .filter_map(move |path| match path {
            Err(e) => Some(Err(e)),
            Ok((path, _)) => {
                use path_slash::PathExt;
                let path_str = path.to_slash_lossy();
                log::debug!("untracked {:?}", path_str);
                if !txn_.read().is_tracked(&path_str).unwrap() {
                    Some(Ok(path))
                } else {
                    None
                }
            }
        }))
}
