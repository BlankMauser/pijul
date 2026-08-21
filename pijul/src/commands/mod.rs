use anyhow::{anyhow, bail};

mod common_opts;

mod init;
pub use init::Init;

mod clone;
pub use clone::Clone;

mod pushpull;
pub use pushpull::*;

mod log;
pub use self::log::Log;

mod record;
pub use record::Record;

mod diff;
pub use diff::Diff;

mod change;
pub use change::Change;

mod http_token;
pub use http_token::HttpToken;

mod status;
pub use status::Status;

mod dependents;
pub use dependents::Dependents;

mod protocol;
pub use protocol::Protocol;

#[cfg(feature = "git")]
mod git;
#[cfg(feature = "git")]
pub use git::Git;

mod channel;
pub use channel::*;

mod reset;
pub use reset::*;

mod fork;
pub use fork::*;

mod unrecord;
pub use unrecord::*;

mod file_operations;
pub use file_operations::*;

mod apply;
pub use apply::*;

mod archive;
pub use archive::*;

mod credit;
pub use credit::*;

mod identity;
pub use identity::*;

mod debug;
pub use debug::*;

mod client;
pub use client::*;

mod completions;
pub use completions::*;

mod docgen;
pub use docgen::*;

mod hooks;
pub use hooks::Hooks;

/// Respect the `pager` key/value pair in both the user's repository config, and their global config.
/// The global configuration requires no additional arguments, but the other two are optional to cover
/// cases in which that information is not available. Users can also disable the pager by not setting
/// the `PAGER` environment variable.
#[cfg(unix)]
fn pager(config: &pijul_config::Config) -> bool {
    if let pijul_config::Choice::Never = config.pager {
        return false;
    } else if let Ok(pager_env_var) = std::env::var("PAGER") {
        if !pager_env_var.is_empty() {
            match pager_env_var.as_str() {
                "less" => {
                    if let Ok(pager_output) = std::process::Command::new(pager_env_var)
                        .args(&["--version"])
                        .output()
                    {
                        let regex = regex::bytes::Regex::new("less ([0-9]+)").unwrap();
                        if let Some(caps) = regex.captures(&pager_output.stdout) {
                            if std::str::from_utf8(&caps[1])
                                .unwrap()
                                .parse::<usize>()
                                .unwrap()
                                >= 530
                            {
                                pager::Pager::with_pager("less -RF").setup();
                                return true;
                            } else {
                                pager::Pager::new().setup();
                            }
                        }
                    }
                }
                owise => {
                    pager::Pager::with_pager(owise).setup();
                }
            }
        }
    }
    false
}

#[cfg(not(unix))]
fn pager(_repo_config_pager: Option<&pijul_config::Choice>) -> bool {
    false
}

use pijul_remote::CS;

/// Make a "changelist", i.e. a list of patches that can be edited in
/// a text editor.
fn make_changelist<S: pijul_core::changestore::ChangeStore>(
    changes: &S,
    pullable: &[CS],
    verb: &str,
) -> Result<Vec<u8>, anyhow::Error> {
    use pijul_core::Base32;
    use std::io::Write;

    let mut v = Vec::new();
    // TODO: This message should probably be customizable
    writeln!(
        v,
        "# Please select the changes to {}. The lines that contain just a
# valid hash, and no other character (except possibly a newline), will
# be {}ed.\n",
        verb, verb,
    )
    .unwrap();
    let mut first_p = true;
    for p in pullable {
        use ::log::*;
        debug!("make_changelist {:?}", p);
        if !first_p {
            writeln!(v, "").unwrap();
        }
        first_p = false;
        let header = match p {
            CS::Change(p) => {
                writeln!(v, "{}\n", p.to_base32()).unwrap();
                let deps = changes.get_dependencies(&p)?;
                if !deps.is_empty() {
                    write!(v, "  Dependencies:").unwrap();
                    for d in deps {
                        write!(v, " {}", d.to_base32()).unwrap();
                    }
                    writeln!(v).unwrap();
                }
                changes.get_header(&p)?
            }
            CS::State(p) => {
                writeln!(v, "{}\n", p.to_base32()).unwrap();
                pijul_core::change::ChangeHeader::default()
            }
        };
        write!(v, "  Author: [").unwrap();
        let mut first = true;
        for a in header.authors.iter() {
            if !first {
                write!(v, ", ").unwrap();
            }
            first = false;
            if let Some(s) = a.0.get("name") {
                write!(v, "{}", s).unwrap()
            } else if let Some(k) = a.0.get("key") {
                write!(v, "{}", k).unwrap()
            }
        }
        writeln!(v, "]").unwrap();
        writeln!(v, "  Date: {}\n", header.timestamp).unwrap();
        for l in header.message.lines() {
            writeln!(v, "    {}", l).unwrap();
        }
        if let Some(desc) = header.description {
            writeln!(v).unwrap();
            for l in desc.lines() {
                writeln!(v, "    {}", l).unwrap();
            }
        }
    }
    Ok(v)
}

/// Parses a list of hashes from a slice of bytes.
/// Everything that is not a line consisting of a
/// valid hash and nothing else will be ignored.
fn parse_changelist(o: &[u8], states: &[CS]) -> Vec<pijul_remote::CS> {
    use pijul_core::Base32;
    let states: std::collections::HashSet<&CS> = states.iter().collect();
    if let Ok(o) = std::str::from_utf8(o) {
        o.lines()
            .filter_map(|l| {
                ::log::debug!(
                    "l = {:?} {:?}",
                    l,
                    pijul_core::Merkle::from_base32(l.as_bytes())
                );
                let h_ = pijul_core::Hash::from_base32(l.as_bytes()).map(pijul_remote::CS::Change);
                if let Some(h) = h_ {
                    if states.contains(&h) {
                        return h_;
                    }
                }
                pijul_core::Merkle::from_base32(&l.as_bytes()[..]).map(pijul_remote::CS::State)
            })
            .collect()
    } else {
        Vec::new()
    }
}

fn find_hash<B: pijul_core::Base32>(
    path: &mut std::path::PathBuf,
    hash: &str,
) -> Result<B, anyhow::Error> {
    if hash.len() < 2 {
        bail!("Ambiguous hash, need at least two characters")
    }
    let (a, b) = hash.split_at(2);
    path.push(a);
    let mut result = None;
    for f in std::fs::read_dir(&path)? {
        let e = f?;
        let p = if let Ok(p) = e.file_name().into_string() {
            p
        } else {
            continue;
        };
        if p.starts_with(b) {
            if result.is_none() {
                result = Some(p)
            } else {
                bail!("Ambiguous hash");
            }
        }
    }
    if let Some(mut r) = result {
        path.push(&r);
        if let Some(i) = r.find('.') {
            r.truncate(i)
        }
        let f = format!("{}{}", a, r);
        if let Some(h) = B::from_base32(f.as_bytes()) {
            return Ok(h);
        }
    }
    bail!("Hash not found")
}

use pijul_core::{ChannelRef, Conflict, TxnT};

// Machine-readable counterpart to `print_conflicts`: emit the conflict list as a
// single JSON object on stdout so scripts/agents (e.g. piclaude land) can drive
// resolution without parsing the human text. Always prints, even when empty, so
// the caller gets a deterministic `{"conflicts":[]}` on a clean pull.
fn print_conflicts_json(conflicts: &[Conflict]) -> Result<(), std::io::Error> {
    use std::io::Write;
    let arr: Vec<serde_json::Value> = conflicts
        .iter()
        .map(|c| match c {
            Conflict::Name { path, .. } => serde_json::json!({"kind": "name", "path": path}),
            Conflict::ZombieFile { path, .. } => {
                serde_json::json!({"kind": "path_deletion", "path": path})
            }
            Conflict::MultipleNames { path, .. } => {
                serde_json::json!({"kind": "multiple_names", "path": path})
            }
            Conflict::Zombie { path, line, .. } => {
                serde_json::json!({"kind": "deletion", "path": path, "line": line})
            }
            Conflict::Cyclic { path, line, .. } => {
                serde_json::json!({"kind": "cycle", "path": path, "line": line})
            }
            Conflict::Order { path, line, .. } => {
                serde_json::json!({"kind": "order", "path": path, "line": line})
            }
        })
        .collect();
    let out = serde_json::json!({ "conflicts": arr });
    let mut w = std::io::stdout();
    writeln!(w, "{}", out)?;
    Ok(())
}

fn print_conflicts(conflicts: &[Conflict]) -> Result<(), std::io::Error> {
    if conflicts.is_empty() {
        return Ok(());
    }
    let mut w = termcolor::StandardStream::stderr(termcolor::ColorChoice::Auto);
    use std::io::Write;
    use termcolor::*;
    w.set_color(ColorSpec::new().set_fg(Some(Color::Red)))?;
    writeln!(w, "\nThere were conflicts:\n")?;
    w.set_color(ColorSpec::new().set_fg(None))?;
    for c in conflicts.iter() {
        match c {
            Conflict::Name { path, .. } => writeln!(w, "  - Name conflict on \"{}\"", path)?,
            Conflict::ZombieFile { path, .. } => {
                writeln!(w, "  - Path deletion conflict \"{}\"", path)?
            }
            Conflict::MultipleNames { path, .. } => {
                writeln!(w, "  - File has multiple names: \"{}\"", path)?
            }
            Conflict::Zombie { path, line, .. } => writeln!(
                w,
                "  - Deletion conflict in \"{}\" starting on line {}",
                path, line
            )?,
            Conflict::Cyclic { path, line, .. } => writeln!(
                w,
                "  - Cycle conflict in \"{}\" starting on line {}",
                path, line
            )?,
            Conflict::Order { path, line, .. } => writeln!(
                w,
                "  - Order conflict in \"{}\" starting on line {}",
                path, line
            )?,
        }
    }
    Ok(())
}

fn get_channel<'a, T>(cli: Option<&'a str>, txn: &'a T) -> (&'a str, bool)
where
    T: TxnT,
{
    let cur = txn.current_channel().ok();
    let ch = cli.or(cur);
    (ch.unwrap_or(pijul_core::DEFAULT_CHANNEL), ch == cur)
}

fn load_channel<T>(cli: Option<&str>, txn: &T) -> anyhow::Result<(ChannelRef<T>, bool)>
where
    T: TxnT,
{
    let (channel_name, is_current_channel) = get_channel(cli, &*txn);
    let channel = load_channel_exact(&channel_name, txn)?;

    Ok((channel, is_current_channel))
}

fn load_channel_exact<T>(name: &str, txn: &T) -> anyhow::Result<ChannelRef<T>>
where
    T: TxnT,
{
    txn.load_channel(name.parse()?)?
        .ok_or_else(|| anyhow!("No such channel: {}", name))
}
