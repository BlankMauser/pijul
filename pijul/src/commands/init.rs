use std::path::{Path, PathBuf};

use clap::{Parser, ValueHint};
use pijul_core::MutTxnT;

use pijul_repository::*;

#[derive(Parser, Debug)]
pub struct Init {
    /// Set the name of the current channel (defaults to "main").
    #[clap(long = "channel")]
    channel: Option<String>,
    /// Project kind; if Pijul knows about your project kind, the .ignore file will be
    /// populated with a conservative list of commonly ignored entries.
    /// Example: `pijul init --kind=rust`
    #[clap(long = "kind", short = 'k')]
    kind: Option<String>,
    /// Path where the repository should be initalized
    #[clap(value_hint = ValueHint::DirPath)]
    path: Option<PathBuf>,
}

impl Init {
    pub fn repository_path(&self) -> Option<&Path> {
        None
    }

    pub fn run(self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let repo = Repository::init(config, self.path.as_deref(), self.kind.as_deref(), None)?;
        let mut txn = repo.pristine.mut_txn_begin()?;
        let channel_name = self
            .channel
            .unwrap_or_else(|| pijul_core::DEFAULT_CHANNEL.to_string());
        let channel_sm: pijul_core::small_string::SmallString = channel_name.parse()?;
        txn.open_or_create_channel(&channel_sm)?;
        txn.set_current_channel(&channel_name)?;
        txn.commit()?;
        Ok(())
    }
}
