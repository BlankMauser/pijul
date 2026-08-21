use clap::{Parser, ValueEnum, ValueHint};
use std::path::{Path, PathBuf};

#[derive(ValueEnum, Clone, Debug)]
pub enum DiffAlgorithm {
    Patience,
    Histogram,
}

/// The output format shared by the read-oriented commands (`log`, `diff`,
/// `status`, `credit`, …). This is the single source of truth for the
/// `--output-format` flag so that every machine-readable client sees a
/// uniform contract.
#[derive(Default, Copy, Clone, Debug, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Plaintext,
    Json,
}

#[derive(Parser)]
pub struct RepoPath {
    /// Work with the repository at PATH instead of the one containing the current directory.
    #[clap(long = "repository", value_name = "PATH", value_hint = ValueHint::DirPath)]
    repo_path: Option<PathBuf>,
    #[clap(skip)]
    cached: Option<pijul_repository::Repository>,
}

impl std::fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoPath")
            .field("repo_path", &self.repo_path)
            .finish_non_exhaustive()
    }
}

#[derive(Parser, Debug)]
pub struct RepoAndChannel {
    #[clap(flatten)]
    base: RepoPath,
    /// Work with CHANNEL instead of the current channel
    #[clap(long = "channel")]
    channel: Option<String>,
}

impl RepoPath {
    /// Returns the repository path. When no explicit path was given, discovers the repository
    /// from the current directory and caches the open `Repository` so `find_root` can reuse it.
    pub fn repo_path(&mut self) -> Option<&Path> {
        if self.cached.is_none() && self.repo_path.is_none() {
            if let Ok(repo) = pijul_repository::Repository::find_root(None) {
                self.repo_path = Some(repo.path.clone());
                self.cached = Some(repo);
            }
        }
        self.repo_path.as_deref()
    }

    /// Returns the open repository. Returns the cached instance from `repo_path()` if available,
    /// otherwise opens it — so `find_root` is called at most once across both methods.
    pub fn find_root(&mut self) -> Result<pijul_repository::Repository, anyhow::Error> {
        if let Some(repo) = self.cached.take() {
            return Ok(repo);
        }
        Ok(pijul_repository::Repository::find_root(
            self.repo_path.as_deref(),
        )?)
    }
}

impl RepoAndChannel {
    pub fn repo_path(&mut self) -> Option<&Path> {
        self.base.repo_path()
    }

    pub fn find_root(&mut self) -> Result<pijul_repository::Repository, anyhow::Error> {
        self.base.find_root()
    }

    pub fn channel(&self) -> Option<&str> {
        self.channel.as_deref()
    }
}
