//! An opt-in log of every `apply` and `unrecord` operation, written to
//! `.pijul/replay.log` so that a sequence of operations (including the
//! ephemeral *pending* patches) can be replayed to reproduce bugs.
//!
//! Enabled by the `replay-log` Cargo feature. When the feature is off, the
//! [`replay_log!`] macro strips its argument entirely, so there is no runtime
//! cost — not even the construction of the [`Op`] value.

#[cfg(feature = "replay-log")]
use pijul_core::pristine::{Base32, Hash};

/// A single logged operation. Borrows its data so logging allocates nothing
/// beyond the JSON line itself.
#[cfg(feature = "replay-log")]
pub enum Op<'a> {
    /// A change applied to a channel (`apply_change*`).
    Apply { channel: &'a str, hash: &'a Hash },
    /// A change unrecorded from a channel.
    Unrecord {
        channel: &'a str,
        hash: &'a Hash,
        salt: u64,
    },
    /// A pending patch (working-copy diff) applied to a channel.
    Pending { channel: &'a str, hash: &'a Hash },
    /// A pending patch unrecorded again — the operation under suspicion.
    UnrecordPending { channel: &'a str, hash: &'a Hash },
}

#[cfg(feature = "replay-log")]
pub fn log(repo_path: &std::path::Path, op: Op<'_>) {
    use std::io::Write;

    let entry = match op {
        Op::Apply { channel, hash } => serde_json::json!({
            "op": "apply",
            "channel": channel,
            "hash": hash.to_base32(),
        }),
        Op::Unrecord {
            channel,
            hash,
            salt,
        } => serde_json::json!({
            "op": "unrecord",
            "channel": channel,
            "hash": hash.to_base32(),
            "salt": salt,
        }),
        Op::Pending { channel, hash } => serde_json::json!({
            "op": "pending",
            "channel": channel,
            "hash": hash.to_base32(),
        }),
        Op::UnrecordPending { channel, hash } => serde_json::json!({
            "op": "unrecord_pending",
            "channel": channel,
            "hash": hash.to_base32(),
        }),
    };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let path = repo_path.join(pijul_core::DOT_DIR).join("replay.log");
    let mut line = format!("{{\"ts\":{ts},");
    // `entry` is always a JSON object; splice its fields in after `ts`.
    line.push_str(&entry.to_string()[1..]);
    line.push('\n');

    if let Err(e) = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)?;
        f.write_all(line.as_bytes())
    })() {
        // Logging must never break a command.
        eprintln!("replay-log: could not write {}: {}", path.display(), e);
    }
}

/// Log an [`Op`] when the `replay-log` feature is enabled; otherwise a no-op
/// whose argument is not even evaluated.
macro_rules! replay_log {
    ($repo_path:expr, $op:expr) => {{
        #[cfg(feature = "replay-log")]
        $crate::replay_log::log($repo_path, $op);
    }};
}

pub(crate) use replay_log;
