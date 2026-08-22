//! Parsing the opaque `<uri>` half of an `ssh:<uri>` machine value.
//!
//! The daemon never interprets this string (see `daemon/src/machines.rs`) --
//! it is handed to this provider verbatim via `--uri`. Three forms must work
//! (RAL-200):
//!
//! - `user@hostname` -- an explicit user.
//! - `hostname` -- falls back to the default user (whatever `ssh` itself
//!   would use: `$USER`/the current OS user, or a `User` line in
//!   `~/.ssh/config`).
//! - A `~/.ssh/config` `Host` alias -- opaque to us either way, since we never
//!   inspect the alias ourselves; `ssh` resolves it.
//!
//! Deliberately thin: this crate does not parse `~/.ssh/config` itself, or
//! validate that a host/alias actually resolves. `ssh` already does both, and
//! duplicating that logic would just be a second, divergent implementation of
//! the same lookup.

use std::fmt;

/// A parsed SSH destination: an optional explicit user plus a host, which may
/// be a real hostname, an IP literal, or a `~/.ssh/config` `Host` alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// Explicit user, when the uri carried a `user@` prefix.
    pub user: Option<String>,
    /// Hostname, IP literal, or ssh-config alias.
    pub host: String,
}

impl SshTarget {
    /// The string to pass as `ssh`'s destination argument: `user@host` when a
    /// user was given, `host` otherwise (letting `ssh` apply its own default,
    /// including any `User` line in `~/.ssh/config` for this alias).
    #[must_use]
    pub fn target_string(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

impl fmt::Display for SshTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.target_string())
    }
}

/// Why a uri could not be parsed as an SSH destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UriParseError {
    /// The uri was empty (or whitespace-only).
    Empty,
    /// A `user@` prefix was present but the user half was empty (`@host`).
    EmptyUser,
    /// The host half was empty (`user@`, or the whole uri after trimming).
    EmptyHost,
    /// The user or host half contained whitespace, which can never be part of
    /// a real hostname/alias/username and almost always means a stray space
    /// crept in when the `machine` value was authored.
    Whitespace(String),
    /// More than one `@` was present, so it is ambiguous which one splits
    /// user from host.
    MultipleAt(String),
}

impl fmt::Display for UriParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty ssh uri: expected \"user@host\" or \"host\""),
            Self::EmptyUser => write!(f, "empty user before '@' in ssh uri"),
            Self::EmptyHost => write!(f, "empty host in ssh uri"),
            Self::Whitespace(raw) => write!(f, "ssh uri {raw:?} must not contain whitespace"),
            Self::MultipleAt(raw) => write!(
                f,
                "ssh uri {raw:?} has more than one '@' -- expected exactly \"user@host\" or \"host\""
            ),
        }
    }
}

/// Parse the `<uri>` half of `ssh:<uri>` into an [`SshTarget`].
///
/// # Errors
/// Returns [`UriParseError`] when the uri is empty, contains whitespace, or
/// has more than one `@`.
pub fn parse(uri: &str) -> Result<SshTarget, UriParseError> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return Err(UriParseError::Empty);
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(UriParseError::Whitespace(trimmed.to_string()));
    }
    let at_count = trimmed.matches('@').count();
    if at_count > 1 {
        return Err(UriParseError::MultipleAt(trimmed.to_string()));
    }
    if let Some((user, host)) = trimmed.split_once('@') {
        if user.is_empty() {
            return Err(UriParseError::EmptyUser);
        }
        if host.is_empty() {
            return Err(UriParseError::EmptyHost);
        }
        Ok(SshTarget {
            user: Some(user.to_string()),
            host: host.to_string(),
        })
    } else {
        Ok(SshTarget {
            user: None,
            host: trimmed.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_at_hostname_parses_both_halves() {
        let t = parse("alice@build-box").unwrap();
        assert_eq!(t.user.as_deref(), Some("alice"));
        assert_eq!(t.host, "build-box");
        assert_eq!(t.target_string(), "alice@build-box");
    }

    #[test]
    fn bare_hostname_has_no_user() {
        let t = parse("build-box").unwrap();
        assert_eq!(t.user, None);
        assert_eq!(t.host, "build-box");
        assert_eq!(t.target_string(), "build-box");
    }

    #[test]
    fn an_ssh_config_alias_is_treated_as_an_opaque_host() {
        // We never look inside ~/.ssh/config -- ssh itself resolves it.
        let t = parse("my-farm-alias").unwrap();
        assert_eq!(t.host, "my-farm-alias");
        assert_eq!(t.target_string(), "my-farm-alias");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let t = parse("  alice@build-box  ").unwrap();
        assert_eq!(t.target_string(), "alice@build-box");
    }

    #[test]
    fn empty_uri_is_rejected() {
        assert_eq!(parse(""), Err(UriParseError::Empty));
        assert_eq!(parse("   "), Err(UriParseError::Empty));
    }

    #[test]
    fn empty_user_before_at_is_rejected() {
        assert_eq!(parse("@build-box"), Err(UriParseError::EmptyUser));
    }

    #[test]
    fn empty_host_after_at_is_rejected() {
        assert_eq!(parse("alice@"), Err(UriParseError::EmptyHost));
    }

    #[test]
    fn internal_whitespace_is_rejected() {
        assert!(matches!(
            parse("alice@build box"),
            Err(UriParseError::Whitespace(_))
        ));
    }

    #[test]
    fn more_than_one_at_is_rejected_as_ambiguous() {
        assert!(matches!(
            parse("alice@bob@build-box"),
            Err(UriParseError::MultipleAt(_))
        ));
    }
}
