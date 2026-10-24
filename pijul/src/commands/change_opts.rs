//! Pieces shared by the commands that write a new change (`record`,
//! `squash`): header options, the change editor, signing, and the names of
//! sub-roots for error messages.

use anyhow::bail;
use clap::Parser;
use jiff::Timestamp;
use pijul_core::change::*;
use pijul_core::changestore::ChangeStore;
use pijul_core::{ChannelRef, DepsTxnT, GraphTxnT, Hash, HashMap, InodeUpdate};

#[derive(Parser, Debug, Default)]
pub struct HeaderOpts {
    /// Set the change message
    #[clap(short = 'm', long = "message")]
    pub message: Option<String>,
    /// Set the description field.
    #[clap(long = "description")]
    pub description: Option<String>,
    /// Set the author field
    #[clap(long = "author")]
    pub author: Option<String>,
    /// Set the timestamp field
    #[clap(long = "timestamp")]
    pub timestamp: Option<Timestamp>,
    /// Identity to sign changes with
    #[clap(long = "identity")]
    pub identity: Option<String>,
}

impl HeaderOpts {
    /// The header of a new change: the command-line fields, else the message
    /// and description templates from the configuration.
    pub async fn header(
        &self,
        config: &pijul_config::Config,
    ) -> Result<ChangeHeader, anyhow::Error> {
        let mut authors = Vec::new();
        let mut b = std::collections::BTreeMap::new();
        if let Some(ref a) = self.author {
            b.insert("name".to_string(), a.clone());
        } else {
            let identity_name = self
                .identity
                .clone()
                .unwrap_or(pijul_identity::choose_identity_name(config).await?);

            let public_key = pijul_identity::public_key(&identity_name);
            b.insert("key".to_string(), public_key?.key);
        }

        authors.push(Author(b));
        let templates = config.template.as_ref();
        let message = if let Some(message) = &self.message {
            message.clone()
        } else if let Some(message_file) = templates.and_then(|t| t.message.as_ref()) {
            match std::fs::read_to_string(message_file) {
                Ok(m) => m,
                Err(e) => bail!("Could not read message template: {:?}: {}", message_file, e),
            }
        } else {
            String::new()
        };
        let description = if let Some(description) = &self.description {
            Some(description.clone())
        } else if let Some(descr_file) = templates.and_then(|t| t.description.as_ref()) {
            match std::fs::read_to_string(descr_file) {
                Ok(d) => Some(d),
                Err(e) => bail!(
                    "Could not read description template: {:?}: {}",
                    descr_file,
                    e
                ),
            }
        } else {
            None
        };
        // Git-style convenience: a "subject\n\nbody" message with no explicit
        // --description splits into message (subject) + description (body).
        // Mirrors `git commit -m` and how agents tend to write messages.
        // NOTE: this changes `-m` semantics for multi-paragraph messages.
        let (message, description) = if description.is_none() {
            split_message(&message)
        } else {
            (message, description)
        };
        Ok(ChangeHeader {
            message,
            authors,
            description,
            timestamp: self.timestamp.unwrap_or_else(Timestamp::now),
        })
    }
}

/// Split a Git-style "subject\n\nbody" message into (subject, Some(body)).
/// Without a blank-line separator the whole string is the subject (no body).
pub fn split_message(msg: &str) -> (String, Option<String>) {
    match msg.split_once("\n\n") {
        Some((subject, body)) if !body.trim().is_empty() => (
            subject.trim_end().to_string(),
            Some(body.trim().to_string()),
        ),
        _ => (msg.to_string(), None),
    }
}

const SYNTAX_ERROR: &str = "# Syntax errors, please try again.
# Alternatively, you may delete the entire file (including this
# comment) to abort.
";

/// Open `change`, as text, in the user's editor (after the `#`-commented
/// `preamble`), until the result parses. `updatables` follows the hunks the
/// user removes.
pub fn edit_change<T, C>(
    change: &Change,
    changes: &C,
    txn: &T,
    channel: &ChannelRef<T>,
    updatables: &mut HashMap<usize, InodeUpdate>,
    preamble: &str,
) -> Result<Change, anyhow::Error>
where
    T: pijul_core::ChannelTxnT + DepsTxnT<DepsError = <T as GraphTxnT>::GraphError>,
    C: ChangeStore,
    C::Error: Send + Sync + 'static,
{
    let mut o = preamble.as_bytes().to_vec();
    change.write(changes, None, true, &mut o)?;

    let mut with_errors: Option<Vec<u8>> = None;
    let change = loop {
        let mut bytes = edit::edit_bytes_with_builder(
            with_errors.as_deref().unwrap_or(&o[..]),
            tempfile::Builder::new().suffix(".pijul-commit"),
        )?;
        if bytes.iter().all(|c| (*c as char).is_whitespace()) {
            bail!("Empty change")
        }
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(&bytes));
        if let Ok(change) = Change::read_and_deps(&mut reader, updatables, txn, channel) {
            break change;
        }

        let mut err = SYNTAX_ERROR.as_bytes().to_vec();
        err.append(&mut bytes);
        with_errors = Some(err)
    };
    if change.changes.is_empty() {
        bail!("Cannot parse change")
    }
    Ok(change)
}

/// Sign `change` with `secret` and save it to `changes`.
pub async fn sign_and_save<C: ChangeStore>(
    change: &mut Change,
    secret: &pijul_core::key::SKey,
    changes: &C,
) -> Result<Hash, anyhow::Error> {
    let hash_for_sig = change.hash()?;
    let sig_pem = pijul_identity::sign_pem(secret, &hash_for_sig.to_bytes()).await?;
    change.unhashed = Some(serde_json::json!({ "signature": sig_pem }));
    Ok(changes.save_change(change, |_, _| Ok::<_, anyhow::Error>(()))?)
}

/// One line per sub-root, for error messages.
pub fn sub_root_names<T: pijul_core::TxnT>(
    txn: &T,
    channel: &T::Channel,
    sub_roots: impl IntoIterator<Item = pijul_core::record::SubRoot>,
) -> Result<String, anyhow::Error> {
    let mut names = String::new();
    for sr in sub_roots {
        match sr {
            pijul_core::record::SubRoot::Existing(pos) => {
                let relocated =
                    pijul_core::pristine::is_relocated_sub_root(txn, txn.graph(channel), pos)
                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                let path = pijul_core::pristine::path_of_sub_root(txn, pos)
                    .map_err(|e| anyhow::anyhow!("{}", e))?
                    .filter(|p| !p.is_empty());
                match (relocated, path) {
                    (true, Some(p)) => names.push_str(&format!("\n  - {} (imported project)", p)),
                    (true, None) => names.push_str("\n  - (imported project)"),
                    (false, Some(p)) => names.push_str(&format!("\n  - {}", p)),
                    (false, None) => names.push_str("\n  - . (main project)"),
                }
            }
            pijul_core::record::SubRoot::New => names.push_str("\n  - (new project)"),
        }
    }
    Ok(names)
}
