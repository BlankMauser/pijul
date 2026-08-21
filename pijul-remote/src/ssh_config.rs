//! Resolving a host against the user's `~/.ssh/config`.
//!
//! `thrussh_config`'s own parser only understands `Host` blocks with a
//! single, literal name and space-separated `Key value` lines. Real
//! configuration files routinely use several patterns on a `Host` line,
//! `*`/`?` wildcards, negated patterns, a catch-all `Host *` block for
//! global defaults and `Key = value` syntax. When `thrussh_config`
//! fails to match, it falls back to defaults (in particular
//! `whoami::username()` for the user), which is why SSH hosts were
//! "almost never parsed right" unless an explicit `user@` was given.
//!
//! This module resolves a host the way OpenSSH does: every `Host` block
//! whose patterns match contributes, and for each key the *first* value
//! seen wins.

use log::debug;
use thrussh_config::{AddKeysToAgent, Config};

/// Resolve `host` against `~/.ssh/config`, returning a [`Config`] whose
/// fields have been filled following OpenSSH's precedence rules. Any
/// field not mentioned by a matching block keeps its default
/// (`whoami::username()` for the user, the host itself for the host
/// name, `22` for the port).
pub fn resolve(host: &str) -> Config {
    let mut config = Config::default(host);
    if let Some(home) = dirs_next::home_dir() {
        let path = home.join(".ssh").join("config");
        match std::fs::read_to_string(&path) {
            Ok(contents) => apply(&mut config, &contents, host),
            Err(e) => debug!("could not read {:?}: {:?}", path, e),
        }
    }
    config
}

/// Apply every matching block of an already-loaded config file to
/// `config`. Factored out of [`resolve`] so it can be unit-tested
/// without touching the filesystem.
fn apply(config: &mut Config, contents: &str, host: &str) {
    // `Config::default` pre-fills `user`, `host_name` and `port`, so we
    // track separately whether a matching block has set them, in order
    // to honour OpenSSH's "first value wins" rule.
    let mut seen_user = false;
    let mut seen_host_name = false;
    let mut seen_port = false;
    let mut seen_add_keys = false;
    let mut matching = false;

    for line in contents.lines() {
        let Some((key, value)) = split_kv(line) else {
            continue;
        };
        if key == "host" {
            matching = host_matches(value, host);
            continue;
        }
        // `Match` blocks are not supported; treat them (and anything
        // until the next `Host`) as non-matching to stay on the safe
        // side rather than misapplying their settings.
        if key == "match" {
            matching = false;
            continue;
        }
        if !matching {
            continue;
        }
        match key.as_str() {
            "hostname" if !seen_host_name => {
                config.host_name = value.to_string();
                seen_host_name = true;
            }
            "user" if !seen_user => {
                config.user = value.to_string();
                seen_user = true;
            }
            "port" if !seen_port => {
                if let Ok(port) = value.parse() {
                    config.port = port;
                    seen_port = true;
                }
            }
            "identityfile" if config.identity_file.is_none() => {
                config.identity_file = Some(expand_tilde(value));
            }
            "proxycommand" if config.proxy_command.is_none() => {
                config.proxy_command = Some(value.to_string());
            }
            "addkeystoagent" if !seen_add_keys => {
                config.add_keys_to_agent = match value.to_lowercase().as_str() {
                    "yes" => AddKeysToAgent::Yes,
                    "confirm" => AddKeysToAgent::Confirm,
                    "ask" => AddKeysToAgent::Ask,
                    _ => AddKeysToAgent::No,
                };
                seen_add_keys = true;
            }
            _ => {}
        }
    }
}

/// Split a configuration line into a lower-cased keyword and its value.
/// Returns `None` for blank lines and comments. Keyword and value may
/// be separated by whitespace, `=`, or both (`Key value`, `Key=value`,
/// `Key = value`), as OpenSSH allows.
fn split_kv(line: &str) -> Option<(String, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let key_end = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let key = line[..key_end].to_lowercase();
    let mut rest = line[key_end..].trim_start();
    if let Some(after_eq) = rest.strip_prefix('=') {
        rest = after_eq.trim_start();
    }
    // Values may be double-quoted (e.g. paths containing spaces).
    let rest = rest
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(rest);
    if rest.is_empty() {
        None
    } else {
        Some((key, rest))
    }
}

/// Whether `host` matches a `Host` line's list of patterns, following
/// OpenSSH semantics: the line matches if `host` matches at least one
/// positive pattern and no negated (`!`) pattern.
fn host_matches(patterns: &str, host: &str) -> bool {
    let mut matched = false;
    for pat in patterns.split_whitespace() {
        if let Some(neg) = pat.strip_prefix('!') {
            if glob_matches(neg, host) {
                return false;
            }
        } else if glob_matches(pat, host) {
            matched = true;
        }
    }
    matched
}

/// Match a single `Host` pattern (with `*` and `?` wildcards) against a
/// host name.
fn glob_matches(pattern: &str, host: &str) -> bool {
    if !pattern.contains(['*', '?']) {
        return pattern == host;
    }
    match regex::Regex::new(&glob_to_regex(pattern)) {
        Ok(re) => re.is_match(host),
        Err(_) => false,
    }
}

fn glob_to_regex(pattern: &str) -> String {
    let mut re = String::with_capacity(pattern.len() + 2);
    re.push('^');
    for c in pattern.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '.' | '^' | '$' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    re.push('$');
    re
}

/// Expand a leading `~/` in an `IdentityFile` value to an absolute path
/// under the home directory, matching what `thrussh_config` did.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_of(contents: &str, host: &str) -> String {
        let mut c = Config::default(host);
        // Neutralise the whoami default so tests are deterministic.
        c.user = "<default>".to_string();
        apply(&mut c, contents, host);
        c.user
    }

    #[test]
    fn single_name() {
        assert_eq!(
            user_of(
                "Host foo\n  HostName foo.example.com\n  User alice\n",
                "foo"
            ),
            "alice"
        );
    }

    #[test]
    fn multiple_patterns_on_host_line() {
        let cfg = "Host foo bar\n  HostName foo.example.com\n  User alice\n";
        assert_eq!(user_of(cfg, "foo"), "alice");
        assert_eq!(user_of(cfg, "bar"), "alice");
        assert_eq!(user_of(cfg, "baz"), "<default>");
    }

    #[test]
    fn wildcard_pattern() {
        let cfg = "Host *.example.com\n  User alice\n";
        assert_eq!(user_of(cfg, "srv.example.com"), "alice");
        assert_eq!(user_of(cfg, "example.org"), "<default>");
    }

    #[test]
    fn question_mark_pattern() {
        let cfg = "Host web?\n  User alice\n";
        assert_eq!(user_of(cfg, "web1"), "alice");
        assert_eq!(user_of(cfg, "web12"), "<default>");
    }

    #[test]
    fn negated_pattern() {
        let cfg = "Host *.example.com !secret.example.com\n  User alice\n";
        assert_eq!(user_of(cfg, "srv.example.com"), "alice");
        assert_eq!(user_of(cfg, "secret.example.com"), "<default>");
    }

    #[test]
    fn global_then_host_first_value_wins() {
        // `Host *` comes first: its User applies to `foo`, and the
        // later block does not override it.
        let cfg = "Host *\n  User alice\nHost foo\n  HostName foo.example.com\n";
        let mut c = Config::default("foo");
        c.user = "<default>".to_string();
        apply(&mut c, cfg, "foo");
        assert_eq!(c.user, "alice");
        assert_eq!(c.host_name, "foo.example.com");
    }

    #[test]
    fn host_specific_wins_over_global_when_earlier() {
        let cfg = "Host foo\n  User bob\nHost *\n  User alice\n";
        assert_eq!(user_of(cfg, "foo"), "bob");
    }

    #[test]
    fn equals_separated() {
        let cfg = "Host foo\n  HostName=foo.example.com\n  User = alice\n";
        let mut c = Config::default("foo");
        c.user = "<default>".to_string();
        apply(&mut c, cfg, "foo");
        assert_eq!(c.user, "alice");
        assert_eq!(c.host_name, "foo.example.com");
    }

    #[test]
    fn port_and_comments() {
        let cfg = "# a comment\nHost foo\n  Port 2222\n  User alice\n";
        let mut c = Config::default("foo");
        apply(&mut c, cfg, "foo");
        assert_eq!(c.port, 2222);
    }
}
