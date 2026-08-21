mod commands;

use clap::{ColorChoice, Parser};
use human_panic::setup_panic;
use pijul_config::{Config, parse_config_arg};
use std::ffi::OsString;
use std::io::Write;

use crate::commands::*;

#[derive(Parser, Debug)]
#[clap(version, author, color(ColorChoice::Auto), infer_subcommands = true)]
pub struct Opts {
    #[clap(subcommand)]
    pub subcmd: SubCommand,
    /// Override a configuration value, in the form `--config key=value`
    #[clap(long, global = true, value_parser = parse_config_arg)]
    pub config: Vec<(String, String)>,
}

#[derive(Parser, Debug)]
pub enum SubCommand {
    /// Initializes an empty pijul repository
    Init(Init),

    /// Clones an existing pijul repository
    Clone(Clone),

    /// Creates a new change
    Record(Record),

    /// Shows difference between two channels/changes
    Diff(Diff),

    /// Shows a brief overview of changes
    Status(Status),

    /// Show the entire log of changes
    Log(Log),

    /// Pushes changes to a remote upstream
    Push(Push),

    /// Pulls changes from a remote upstream
    Pull(Pull),

    /// Shows information about a particular change
    Change(Change),

    /// Mints an HTTP bearer token for a Nest using your SSH key
    HttpToken(HttpToken),

    /// Lists the transitive closure of the reverse dependency relation
    Dependents(Dependents),

    /// Manages different channels
    Channel(Channel),

    #[clap(hide = true)]
    Protocol(Protocol),

    #[cfg(feature = "git")]
    /// Imports a git repository into pijul
    Git(Git),

    /// Moves a file in the working copy and the tree
    #[clap(alias = "mv")]
    Move(Move),

    /// Lists files tracked by pijul
    #[clap(alias = "ls")]
    List(List),

    /// Adds a path to the tree.
    ///
    /// Pijul has an internal tree to represent the files currently
    /// tracked. This command adds files and directories to that tree.
    Add(Add),

    /// Removes a file from the tree of tracked files (`pijul record`
    /// will then record this as a deletion).
    #[clap(alias = "rm")]
    Remove(Remove),

    /// Resets the working copy to the last recorded change.
    ///
    /// In other words, discards all unrecorded changes.
    Reset(Reset),

    // #[cfg(debug_assertions)]
    Debug(Debug),

    /// Create a new channel
    Fork(Fork),

    /// Unrecords a list of changes.
    ///
    /// The changes will be removed from your log, but your working
    /// copy will stay exactly the same, unless the
    /// `--reset` flag was passed. A change can only be unrecorded
    /// if all changes that depend on it are also unrecorded in the
    /// same operation. There are two ways to call `pijul-unrecord`:
    ///
    /// * With a list of <change-id>s. The given changes will be
    /// unrecorded, if possible.
    ///
    /// * Without listing any <change-id>s. You will be
    /// presented with a list of changes to choose from.
    /// The length of the list is determined by the `unrecord_changes`
    /// setting in your global config or the `--show-changes` option,
    /// with the latter taking precedence.
    Unrecord(Unrecord),

    /// Applies changes to a channel
    Apply(Apply),

    /// Manages remote repositories
    Remote(Remote),

    /// Creates an archive of the repository
    Archive(Archive),

    /// Shows which change last affected each line of the given file(s)
    Credit(Credit),

    /// A collection of tools for interactively managing the user's identities.
    /// This may be useful if you use Pijul in multiple contexts, for example
    /// both work & personal projects.
    #[clap(alias = "id", alias = "key")]
    Identity(IdentityCommand),

    /// Authenticates with a HTTP server.
    Client(Client),

    /// Manage the repository-wide shared hooks declared in `pijul.toml`
    Hooks(Hooks),

    /// Shell completion script generation
    Completion(Completion),

    /// Generate man pages or markdown documentation
    Docgen(Docgen),

    #[clap(external_subcommand)]
    ExternalSubcommand(Vec<OsString>),
}

#[test]
/// Make sure all clap derive macros are (reasonably) correct
fn clap_debug_assert() {
    use clap::CommandFactory;
    Opts::command().debug_assert();
}

#[tokio::main]
async fn main() {
    setup_panic!();

    let mut builder = env_logger::builder();
    builder.filter(Some("pijul::commands::git"), log::LevelFilter::Info);
    builder.init();

    let opts = Opts::parse();
    pijul_interaction::set_context(pijul_interaction::InteractiveContext::NotInteractive);

    if let Err(e) = run(opts).await {
        // This will only activate with the following environment variables:
        // RUST_BACKTRACE=1 RUST_LOG=error
        log::error!("Error: {:#?}", e);
        match e.downcast::<std::io::Error>() {
            Ok(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Ok(e) => writeln!(std::io::stderr(), "Error: {}", e).unwrap_or(()),
            Err(e) => writeln!(std::io::stderr(), "Error: {}", e).unwrap_or(()),
        }
        std::process::exit(1);
    } else {
        std::process::exit(0);
    }
}

#[cfg(unix)]
fn run_external_command(mut command: Vec<OsString>) -> Result<(), std::io::Error> {
    let args = command.split_off(1);
    let mut cmd: OsString = "pijul-".into();
    cmd.push(&command[0]);

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&cmd).args(args).exec();
    report_external_command_error(&command[0], err);
}

#[cfg(windows)]
fn run_external_command(mut command: Vec<OsString>) -> Result<(), std::io::Error> {
    let args = command.split_off(1);
    let mut cmd: OsString = "pijul-".into();
    cmd.push(&command[0]);

    let mut spawned = match std::process::Command::new(&cmd).args(args).spawn() {
        Ok(spawned) => spawned,
        Err(e) => {
            report_external_command_error(&command[0], e);
        }
    };
    let status = spawned.wait()?;
    std::process::exit(status.code().unwrap_or(1))
}

fn report_external_command_error(cmd: &OsString, err: std::io::Error) -> ! {
    match err.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
            writeln!(std::io::stderr(), "No such subcommand: {:?}", cmd).unwrap_or(())
        }
        _ => writeln!(std::io::stderr(), "Error while running {:?}: {}", cmd, err).unwrap_or(()),
    }
    std::process::exit(1)
}

async fn run(mut opts: Opts) -> Result<(), anyhow::Error> {
    let repository_path = match &mut opts.subcmd {
        SubCommand::Log(log) => log.repository_path(),
        SubCommand::Init(init) => init.repository_path(),
        SubCommand::Clone(clone) => clone.repository_path(),
        SubCommand::Record(record) => record.repository_path(),
        SubCommand::Diff(diff) => diff.repository_path(),
        SubCommand::Status(status) => status.repository_path(),
        SubCommand::Push(push) => push.repository_path(),
        SubCommand::Pull(pull) => pull.repository_path(),
        SubCommand::Change(change) => change.repository_path(),
        SubCommand::HttpToken(http_token) => http_token.repository_path(),
        SubCommand::Dependents(deps) => deps.repository_path(),
        SubCommand::Channel(channel) => channel.repository_path(),
        SubCommand::Protocol(protocol) => protocol.repository_path(),
        #[cfg(feature = "git")]
        SubCommand::Git(git) => git.repository_path(),
        SubCommand::Move(move_cmd) => move_cmd.repository_path(),
        SubCommand::List(list) => list.repository_path(),
        SubCommand::Add(add) => add.repository_path(),
        SubCommand::Remove(remove) => remove.repository_path(),
        SubCommand::Reset(reset) => reset.repository_path(),
        SubCommand::Debug(debug) => debug.repository_path(),
        SubCommand::Fork(fork) => fork.repository_path(),
        SubCommand::Unrecord(unrecord) => unrecord.repository_path(),
        SubCommand::Apply(apply) => apply.repository_path(),
        SubCommand::Remote(remote) => remote.repository_path(),
        SubCommand::Archive(archive) => archive.repository_path(),
        SubCommand::Credit(credit) => credit.repository_path(),
        SubCommand::Identity(identity) => identity.repository_path(),
        SubCommand::Client(client) => client.repository_path(),
        SubCommand::Hooks(hooks) => hooks.repository_path(),
        SubCommand::ExternalSubcommand(_) | SubCommand::Completion(_) | SubCommand::Docgen(_) => {
            None
        }
    };

    let config = Config::load(repository_path, opts.config)?;

    log::debug!("{config:#?}");

    match opts.subcmd {
        SubCommand::Log(l) => l.run(&config),
        SubCommand::Init(init) => init.run(&config),
        SubCommand::Clone(clone) => clone.run(&config).await,
        SubCommand::Record(record) => record.run(&config).await,
        SubCommand::Diff(diff) => diff.run(&config),
        SubCommand::Status(status) => status.run(&config),
        SubCommand::Push(push) => push.run(&config).await,
        SubCommand::Pull(pull) => pull.run(&config).await,
        SubCommand::Change(change) => change.run(&config),
        SubCommand::HttpToken(http_token) => http_token.run(&config).await,
        SubCommand::Dependents(deps) => deps.run(),
        SubCommand::Channel(channel) => channel.run(),
        SubCommand::Protocol(protocol) => protocol.run(),
        #[cfg(feature = "git")]
        SubCommand::Git(git) => git.run(&config),
        SubCommand::Move(move_cmd) => move_cmd.run(),
        SubCommand::List(list) => list.run(),
        SubCommand::Add(add) => add.run(),
        SubCommand::Remove(remove) => remove.run(),
        SubCommand::Reset(reset) => reset.run(&config),
        SubCommand::Debug(debug) => debug.run(),
        SubCommand::Fork(fork) => fork.run(),
        SubCommand::Unrecord(unrecord) => unrecord.run(&config),
        SubCommand::Apply(apply) => apply.run(),
        SubCommand::Remote(remote) => remote.run(&config),
        SubCommand::Archive(archive) => archive.run(&config).await,
        SubCommand::Credit(credit) => credit.run(&config),
        SubCommand::Identity(identity_wizard) => identity_wizard.run(&config).await,
        SubCommand::Client(client) => client.run().await,
        SubCommand::Hooks(hooks) => hooks.run(&config),
        SubCommand::ExternalSubcommand(command) => Ok(run_external_command(command)?),
        SubCommand::Completion(completion) => completion.run(),
        SubCommand::Docgen(docgen) => docgen.run(),
    }
}
