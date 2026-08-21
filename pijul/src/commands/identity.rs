use crate::commands::common_opts::RepoPath;
use jiff::tz::TimeZone;
use pijul_config::author::Author;
use pijul_identity::{self as identity, Complete, choose_identity_name, fix_identities};
use pijul_remote as remote;

use std::io::Write;
use std::path::Path;

use clap::Parser;
use jiff::Timestamp;
use log::{info, warn};
use ptree::{TreeBuilder, print_tree};

mod subcmd {
    use anyhow::{anyhow, bail};
    use clap::{ArgGroup, Parser, ValueHint};
    use jiff::Timestamp;

    fn validate_email(input: &str) -> Result<String, anyhow::Error> {
        use validator::ValidateEmail;

        if input.validate_email() {
            Ok(input.to_string())
        } else {
            bail!("Invalid email address");
        }
    }

    fn valid_name(input: &str) -> Result<(), anyhow::Error> {
        if input.is_empty() {
            bail!("Name is empty");
        } else {
            Ok(())
        }
    }

    fn name_does_not_exist(input: &str) -> Result<String, anyhow::Error> {
        valid_name(&input)?;

        if pijul_identity::Complete::load(input).is_ok() {
            bail!("Name already exists")
        } else {
            Ok(input.to_string())
        }
    }

    fn name_exists(input: &str) -> Result<String, anyhow::Error> {
        valid_name(&input)?;

        if pijul_identity::Complete::load(input).is_err() {
            bail!("Name does not exist");
        } else {
            Ok(input.to_string())
        }
    }

    fn parse_expiry(input: &str) -> Result<Timestamp, anyhow::Error> {
        let parsed_date: Timestamp = input
            .parse()
            .map_err(|error| anyhow!("Error parsing date: {error:#?}"))?;

        if parsed_date <= Timestamp::now() {
            Err(anyhow!("Date is in the past"))
        } else {
            Ok(parsed_date)
        }
    }

    #[derive(Clone, Parser, Debug)]
    #[clap(group(
        ArgGroup::new("edit_data")
            .multiple(true)
            .args(&["display_name", "email", "expiry", "username", "remote", "name"]),
    ))]
    pub struct New {
        /// Do not automatically link keys with the remote
        #[clap(long = "no-link", display_order = 1)]
        pub no_link: bool,
        /// Set the username
        #[clap(long = "username", display_order = 3, value_hint = ValueHint::Username)]
        pub username: Option<String>,
        /// Set the default remote
        #[clap(long = "remote", display_order = 3)]
        pub remote: Option<String>,
        /// Set the display name
        #[clap(long = "display-name", display_order = 3)]
        pub display_name: Option<String>,
        /// Set the email
        #[clap(long = "email", value_parser = validate_email, display_order = 3)]
        pub email: Option<String>,
        /// Set the new identity name
        #[clap(value_parser = name_does_not_exist)]
        pub name: Option<String>,
        /// Set the expiry
        #[clap(long = "expiry", value_parser = parse_expiry, display_order = 3)]
        pub expiry: Option<Timestamp>,
    }

    #[derive(Clone, Parser, Debug)]
    #[clap(group(
        ArgGroup::new("edit_data")
            .multiple(true)
            .args(&["display_name", "email", "new_name", "expiry", "username", "remote"]),
    ))]
    pub struct Edit {
        /// Set the name of the identity to edit
        #[clap(group("name"), value_parser = name_exists)]
        pub old_name: Option<String>,
        /// Do not automatically link keys with the remote
        #[clap(long = "no-link", display_order = 1)]
        pub no_link: bool,
        /// Set the username
        #[clap(long = "username", display_order = 3, value_hint = ValueHint::Username)]
        pub username: Option<String>,
        /// Set the default remote
        #[clap(long = "remote", display_order = 3)]
        pub remote: Option<String>,
        /// Set the display name
        #[clap(long = "display-name", display_order = 3)]
        pub display_name: Option<String>,
        /// Set the email
        #[clap(long = "email", value_parser = validate_email, display_order = 3)]
        pub email: Option<String>,
        /// Set the identity name
        #[clap(long = "new-name", display_order = 3)]
        pub new_name: Option<String>,
        /// Set the expiry
        #[clap(long = "expiry", value_parser = parse_expiry, display_order = 3)]
        pub expiry: Option<Timestamp>,
    }
}

#[derive(Clone, Parser, Debug)]
pub enum SubCommand {
    /// Create a new identity
    New(subcmd::New),
    /// Repair the identity state on disk, including migration from older versions of Pijul
    Repair,
    /// Prove an identity to the server
    Prove {
        /// Set the name used to prove the identity
        #[clap(long = "name")]
        identity_name: Option<String>,
        /// Set the target server
        server: Option<String>,
    },
    /// Pretty-print all valid identities on disk
    List,
    /// Edit an existing identity
    Edit(subcmd::Edit),
    /// Remove an existing identity
    #[clap(alias = "rm")]
    Remove {
        /// Set the name of the identity to remove
        #[clap(long = "name")]
        identity_name: Option<String>,
    },
}

#[derive(Parser, Debug)]
pub struct IdentityCommand {
    #[clap(subcommand)]
    subcmd: SubCommand,
    #[clap(flatten)]
    repository_path: RepoPath,
    /// Do not verify certificates (use with caution)
    #[clap(long = "no-cert-check", short = 'k')]
    no_cert_check: bool,
}

fn unwrap_args(
    default: Complete,
    identity_name: Option<String>,
    login: Option<String>,
    display_name: Option<String>,
    origin: Option<String>,
    email: Option<String>,
    expiry: Option<Timestamp>,
) -> Result<Complete, anyhow::Error> {
    let mut public_key = default.public_key;
    public_key.expires = expiry;

    Ok(Complete::new(
        identity_name.unwrap_or(default.name),
        identity::IdentityConfig {
            key_path: None,
            author: Author {
                username: login.unwrap_or(default.config.author.username),
                display_name: display_name.unwrap_or(default.config.author.display_name),
                email: email.unwrap_or(default.config.author.email),
                origin: origin.unwrap_or(default.config.author.origin),
                key_path: None,
            },
        },
        public_key,
    ))
}

impl IdentityCommand {
    pub fn repository_path(&mut self) -> Option<&Path> {
        self.repository_path.repo_path()
    }

    pub async fn run(self, config: &pijul_config::Config) -> Result<(), anyhow::Error> {
        let mut stderr = std::io::stderr();

        match self.subcmd {
            SubCommand::New(options) => {
                let identity = unwrap_args(
                    Complete::default(config)?,
                    options.name,
                    options.username,
                    options.display_name,
                    options.remote,
                    options.email,
                    options.expiry,
                )?;

                identity.create(config).await?;

                if let Err(_) = remote::prove(config, &identity, None, self.no_cert_check).await {
                    warn!(
                        "Could not prove identity `{}`. Please check your credentials & network connection. If you are on an enterprise network, perhaps try running with `--no-cert-check`. Your data is safe but will not be connected to {} without runnning `pijul identity prove {}`",
                        identity.name, identity.config.author.origin, identity.name
                    );
                } else {
                    info!("Identity `{}` was proved to the server", identity);
                }
            }
            SubCommand::Repair => fix_identities(config).await?,
            SubCommand::Prove {
                identity_name,
                server,
            } => {
                let identity_name = &identity_name.unwrap_or(choose_identity_name(config).await?);
                let loaded_identity = Complete::load(identity_name)?;
                remote::prove(
                    config,
                    &loaded_identity,
                    server.as_deref(),
                    self.no_cert_check,
                )
                .await?;
            }
            SubCommand::List => {
                let identities = Complete::load_all()?;

                if identities.is_empty() {
                    let mut stderr = std::io::stderr();
                    writeln!(
                        stderr,
                        "No identities found. Use `pijul identity new` to create one."
                    )?;
                    writeln!(
                        stderr,
                        "If you have created a key in the past, you may need to migrate via `pijul identity repair`"
                    )?;

                    return Ok(());
                }

                let mut tree = TreeBuilder::new("Identities".to_string());
                for identity in identities {
                    tree.begin_child(identity.name.clone());

                    tree.add_empty_child(format!(
                        "Display name: {}",
                        if identity.config.author.display_name.is_empty() {
                            "<NO NAME>"
                        } else {
                            &identity.config.author.display_name
                        }
                    ));

                    tree.add_empty_child(format!(
                        "Email: {}",
                        if identity.config.author.email.is_empty() {
                            "<NO EMAIL>"
                        } else {
                            &identity.config.author.email
                        }
                    ));

                    let username = if identity.config.author.username.is_empty() {
                        "<NO USERNAME>"
                    } else {
                        &identity.config.author.username
                    };

                    let origin = if identity.config.author.origin.is_empty() {
                        "<NO ORIGIN>"
                    } else {
                        &identity.config.author.origin
                    };
                    tree.add_empty_child(format!("Login: {username}@{origin}"));

                    tree.begin_child("Public key".to_string());
                    tree.add_empty_child(format!(
                        "Fingerprint: {}",
                        identity.public_key.fingerprint()
                    ));
                    tree.add_empty_child(format!(
                        "Expiry: {}",
                        identity
                            .public_key
                            .expires
                            .map(|date| date.strftime("%Y-%m-%d %H:%M:%S (%Z)").to_string())
                            .unwrap_or_else(|| "Never".to_string())
                    ));
                    tree.end_child();

                    tree.add_empty_child(format!(
                        "Last updated: {}",
                        identity
                            .last_modified
                            .to_zoned(TimeZone::system())
                            .strftime("%Y-%m-%d %H:%M:%S (%Z)")
                    ));
                    tree.end_child();
                }

                print_tree(&tree.build())?;
            }
            SubCommand::Edit(options) => {
                let old_id_name = if let Some(id_name) = options.old_name {
                    id_name
                } else {
                    choose_identity_name(config).await?
                };
                writeln!(std::io::stderr(), "Editing identity: {old_id_name}")?;

                let old_identity = Complete::load(&old_id_name)?;
                let cli_args = unwrap_args(
                    old_identity.clone(),
                    options.new_name,
                    options.username,
                    options.display_name,
                    options.remote,
                    options.email,
                    options.expiry,
                )?;

                let new_identity = cli_args;
                old_identity.clone().replace_with(new_identity.clone())?;

                if !options.no_link {
                    if old_identity.public_key != new_identity.public_key
                        || old_identity.config.author != new_identity.config.author
                    {
                        let prove_result =
                            remote::prove(config, &new_identity, None, self.no_cert_check).await;

                        if let Err(_) = prove_result {
                            warn!(
                                "Could not prove identity `{}`. Please check your credentials & network connection. If you are on an enterprise network, perhaps try running with `--no-cert-check`. Your data is safe but will not be connected to {} without runnning `pijul identity prove {}`",
                                new_identity.name,
                                new_identity.config.author.origin,
                                new_identity.name
                            );
                        } else {
                            info!("Identity `{}` was proved to the server", new_identity);
                        }
                    }
                }
            }
            SubCommand::Remove { identity_name } => {
                if Complete::load_all()?.is_empty() {
                    writeln!(stderr, "No identities to remove!")?;
                    return Ok(());
                }

                let identity =
                    Complete::load(&identity_name.unwrap_or(choose_identity_name(config).await?))?;

                let path = pijul_config::global_config_directory()?
                    .join("identities")
                    .join(&identity.name);

                std::fs::remove_dir_all(&path)?;
                writeln!(stderr, "Removed identity: {identity} at {path:?}")?;
            }
        }

        Ok(())
    }
}
