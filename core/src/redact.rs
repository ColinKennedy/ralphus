//! Generic redaction of credential env-var values (RAL-247).
//!
//! Guarantees that the *values* of auth/credential environment variables
//! (e.g. `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`,
//! and any provider credentials) are never echoed to a terminal, written to a
//! transcript, or shown in any user-visible view. The daemon delivers squad
//! env-overrides to a tmux-wrapped runner on Windows by inlining them into the
//! typed shell line (see
//! `ralphus_daemon::tmux::new_detached_session_with_command`) — PowerShell
//! echoes that whole line into the pane as text, so on a pane-capture the
//! secret shows up as a `$env:KEY = '<value>'` assignment. This module is the
//! single redaction getter (`[`redact_secrets`]`) that every read/serve/log
//! path calls to scrub such assignments, and the predicate
//! (`[`is_secret_env_key`]`) that decides which env names are secrets.
//!
//! It lives in `ralphus-core` (the dependency-light shared crate) so the
//! daemon, the CLI, and the runner can all call the *same* function rather
//! than drifting copies of the rule.

use std::borrow::Cow;

/// The literal inserted where a secret value was masked.
pub const REDACTED: &str = "[REDACTED]";

/// The literal `ralphus-daemon`'s exact-value secret registry (RAL-264,
/// `daemon::redact::REDACTED`) inserts where a *registered* secret value was
/// masked. Duplicated here rather than imported — `ralphus-core` has no
/// dependency on the daemon — so an assignment value that the registry has
/// already scrubbed is recognized and left alone instead of being re-masked
/// with [`REDACTED`], which would destroy the more specific registry
/// placeholder for no security benefit (the value is already gone either
/// way).
const ALREADY_REDACTED_BY_REGISTRY: &str = "<redacted>";

/// Env-var *name* substrings that mark a variable as a credential, matched
/// case-insensitively against an uppercased key. `_KEY` (not bare `KEY`) is
/// used so e.g. `ANTHROPIC_API_KEY` matches while a name like `MONKEY`
/// doesn't. These cover most provider credentials regardless of vendor, so a
/// new API key/token is caught without adding it to a hand-maintained list.
const SECRET_KEY_SUBSTRINGS: &[&str] =
    &["TOKEN", "_KEY", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"];

/// Explicitly-known auth env-var names that the substring heuristic doesn't
/// catch but that must still never be echoed (RAL-247 names
/// `ANTHROPIC_BASE_URL`, which is auth-related but carries no
/// TOKEN/KEY/SECRET/… marker in its name).
const KNOWN_AUTH_KEYS: &[&str] = &["ANTHROPIC_BASE_URL"];

/// Whether `key` is an auth/credential environment variable whose *value*
/// must never be echoed or persisted — the predicate behind [`redact_secrets`].
/// Explicit known names plus a generic name heuristic, so new provider
/// credentials are covered without a maintained list. Over-matching a
/// non-secret name is safe (it just over-redacts); under-matching is the bug.
#[must_use]
pub fn is_secret_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    KNOWN_AUTH_KEYS.iter().any(|k| *k == upper)
        || SECRET_KEY_SUBSTRINGS.iter().any(|s| upper.contains(s))
}

/// Redact every secret env-var *value* in `text`, replacing each with
/// [`REDACTED`]. Recognizes the assignment forms the runner's command-line
/// builder produces — PowerShell `$env:KEY = '<value>'` (the Windows
/// `send-keys` echo that was the concrete RAL-247 leak) and the POSIX
/// `KEY='<value>'` / `KEY=<value>` prefix form — plus the same structures
/// embedded anywhere else in a transcript.
///
/// Returns `Cow::Borrowed` (no copy, no allocation) when the text contains
/// nothing that looks like a secret assignment, so hot read paths like a
/// frequently-polled live pane don't pay for a rebuild when there's nothing
/// to scrub.
#[must_use]
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    if !text_contains_suspect(text) {
        return Cow::Borrowed(text);
    }
    let mut masked = false;
    let pwsh = redact_pwsh_env_assignments(text, &mut masked);
    let out = redact_posix_env_assignments(&pwsh, &mut masked);
    if masked {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// Fast pre-filter: only run the (linear) scanners when the text could
/// plausibly contain a secret env assignment — a `$env:` marker, or a secret
/// name substring alongside an `=` (without an `=`, there's no value to
/// mask).
fn text_contains_suspect(text: &str) -> bool {
    if text.contains("$env:") {
        return true;
    }
    if !text.contains('=') {
        return false;
    }
    let upper = text.to_ascii_uppercase();
    KNOWN_AUTH_KEYS.iter().any(|k| upper.contains(k))
        || SECRET_KEY_SUBSTRINGS.iter().any(|s| upper.contains(s))
}

/// Redact PowerShell `$env:KEY = '<value>'` (and unquoted `$env:KEY = value`)
/// assignments. Scans one pass, emitting the (possibly redacted) result into
/// a fresh string; sets `masked` when any value was replaced.
fn redact_pwsh_env_assignments(text: &str, masked: &mut bool) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'$' && text[i..].starts_with("$env:") {
            let mut j = i + 5; // skip "$env:"
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let key = &text[i + 5..j];
            if !key.is_empty() && is_secret_env_key(key) {
                if let Some(consumed) = redact_assignment_value(text, b, i, j, &mut out, masked) {
                    i = consumed;
                    continue;
                }
            }
        }
        let ch = text[i..].chars().next().expect("i is a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Redact POSIX `KEY='<value>'` / `KEY = value` prefix assignments (e.g.
/// `ANTHROPIC_AUTH_TOKEN='sk-…' ralphus-runner …`). Not the Windows leak path
/// (POSIX `respawn-pane` never echoes) but kept cross-platform so the getter
/// is a single scrubber everywhere.
fn redact_posix_env_assignments(text: &str, masked: &mut bool) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < b.len() {
        let at_boundary = i == 0 || !is_id_byte(b[i - 1]);
        if at_boundary && is_id_start(b[i]) {
            let mut j = i;
            while j < b.len() && is_id_byte(b[j]) {
                j += 1;
            }
            let key = &text[i..j];
            // Only revisit an identifier when it's secret; otherwise copy the
            // whole token and skip past it so the scan stays O(n).
            if is_secret_env_key(key) {
                if let Some(consumed) = redact_assignment_value(text, b, i, j, &mut out, masked) {
                    i = consumed;
                    continue;
                }
            }
            out.push_str(&text[i..j]);
            i = j;
            continue;
        }
        let ch = text[i..].chars().next().expect("i is a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Open exactly one quoted/unquoted value immediately after the identifier
/// span `[id_start, id_end)` and, if it is a secret assignment, redact the
/// value into `out`. On success returns the byte offset to resume scanning at
/// (just after the closed value); on any non-assignment returns `None` so the
/// caller copies the identifier verbatim.
fn redact_assignment_value(
    text: &str,
    b: &[u8],
    id_start: usize,
    id_end: usize,
    out: &mut String,
    masked: &mut bool,
) -> Option<usize> {
    // Expect optional whitespace, then '=', then optional whitespace.
    let mut k = id_end;
    while k < b.len() && (b[k] == b' ' || b[k] == b'\t') {
        k += 1;
    }
    if k >= b.len() || b[k] != b'=' {
        return None;
    }
    let mut v = k + 1;
    while v < b.len() && (b[v] == b' ' || b[v] == b'\t') {
        v += 1;
    }
    if v >= b.len() {
        return None;
    }
    if b[v] == b'\'' {
        // Single-quoted value. PowerShell doubles '' for a literal quote; the
        // string closes on a ' not followed by another '. POSIX additionally
        // represents a literal quote as the 4-char escape '\'' (quote,
        // backslash, quote, quote), handled by the exact-match branch below —
        // only that exact sequence is consumed so a PowerShell value carrying
        // a backslash (e.g. a path) is never misparsed.
        let mut p = v + 1;
        let mut value_end = None;
        while p < b.len() {
            if b[p] == b'\'' {
                if p + 3 < b.len() && b[p + 1] == b'\\' && b[p + 2] == b'\'' && b[p + 3] == b'\'' {
                    p += 4; // POSIX '\'' escape (one literal quote)
                    continue;
                }
                if p + 1 < b.len() && b[p + 1] == b'\'' {
                    p += 2; // '' = literal quote (PowerShell doubling)
                    continue;
                }
                value_end = Some(p);
                break;
            } else if b[p] == b'\n' || b[p] == b'\r' {
                break; // unterminated in this line; leave it unmasked
            }
            p += 1;
        }
        let p = value_end?;
        if &text[v + 1..p] == ALREADY_REDACTED_BY_REGISTRY {
            return None;
        }
        out.push_str(&text[id_start..=v]); // includes the opening quote
        out.push_str(REDACTED);
        out.push('\'');
        *masked = true;
        Some(p + 1)
    } else {
        // Unquoted value: extends to the first ';', whitespace, or EOL.
        let mut p = v;
        while p < b.len()
            && b[p] != b';'
            && b[p] != b' '
            && b[p] != b'\t'
            && b[p] != b'\n'
            && b[p] != b'\r'
        {
            p += 1;
        }
        if &text[v..p] == ALREADY_REDACTED_BY_REGISTRY {
            return None;
        }
        out.push_str(&text[id_start..v]);
        out.push_str(REDACTED);
        *masked = true;
        Some(p)
    }
}

fn is_id_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn is_id_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

/// The literal inserted in place of a URL's redacted inline credentials.
pub const REDACTED_URL_CREDENTIAL: &str = "***";

/// Split `url`'s authority component (the `user:pass@host[:port]` segment
/// right after `scheme://`) out, if it has one. Returns `(scheme_and_slashes,
/// authority, rest)` so callers can rebuild the string around whichever part
/// they need to inspect or replace, without re-parsing.
fn split_authority(url: &str) -> Option<(&str, &str, &str)> {
    let scheme_end = url.find("://")?;
    let (prefix, after) = url.split_at(scheme_end + 3);
    let authority_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (authority, rest) = after.split_at(authority_end);
    Some((prefix, authority, rest))
}

/// True when `url` is an `http(s)://` project clone URL with a password
/// inlined in its authority (`https://user:pass@host/repo.git`) — a real
/// project-registration project clone URL should never carry one, since a
/// leaked project registry entry (a Cartographer payload, a shared board, a
/// support bundle) would then leak the password too. An SSH-shorthand URL
/// (`git@host:org/repo.git`) or a bare userinfo without a password
/// (`https://git@host/repo.git`, common for token-as-username hosting) does
/// not trip this — only `scheme://user:pass@` shapes carry an inline secret.
#[must_use]
pub fn https_url_has_embedded_password(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return false;
    }
    let Some((_, authority, _)) = split_authority(url) else {
        return false;
    };
    match authority.rfind('@') {
        Some(at) => authority[..at].contains(':'),
        None => false,
    }
}

/// Replace inline `user:pass@`/`user@` credentials in `url`'s authority with
/// [`REDACTED_URL_CREDENTIAL`], leaving the scheme, host, port, and path
/// intact. Returns `url` unchanged when it has no `scheme://…@…` shape (e.g.
/// an SSH shorthand, or a URL with no userinfo at all) — there is nothing to
/// redact in that case.
#[must_use]
pub fn redact_url_credentials(url: &str) -> String {
    let Some((prefix, authority, rest)) = split_authority(url) else {
        return url.to_string();
    };
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    format!(
        "{prefix}{REDACTED_URL_CREDENTIAL}@{}{rest}",
        &authority[at + 1..]
    )
}

/// One environment variable as a read-only viewer shows it (RAL-324).
///
/// Produced only by [`redact_env_map`], so every caller — the daemon's HTTP
/// API, the board's env-viewer popup, and `ralphus env` — displays the same
/// masked value for the same variable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EnvVarDisplay {
    /// The variable's name, never masked.
    pub name: String,
    /// The value as it may be displayed — [`REDACTED`] when `redacted`.
    pub value: String,
    /// Whether `name` is registered as secret, so `value` is the placeholder
    /// rather than the real value.
    pub redacted: bool,
}

/// Mask one environment variable's value for display (RAL-324).
///
/// `secret_names` is the daemon's user-registered secret env-var **name**
/// list (RAL-281's Secrets tab, `ralphus_daemon::secret_env_names`), which is
/// the only thing that decides whether a variable is secret here: an exact
/// name match masks the whole value with [`REDACTED`]. Deliberately *not*
/// [`is_secret_env_key`] — a viewer that hid variables the Secrets tab does
/// not list would disagree with what the same list scrubs from pane text.
///
/// A non-secret value still goes through [`redact_secrets`], which catches a
/// credential assignment embedded *inside* the value (e.g. a wrapper command
/// stored as `sh -c "ANTHROPIC_API_KEY=sk-… run"`). Returns the displayable
/// value and whether the name match masked it outright.
#[must_use]
pub fn redact_env_value(
    name: &str,
    value: &str,
    secret_names: &std::collections::BTreeSet<String>,
) -> (String, bool) {
    if secret_names.contains(name) {
        return (REDACTED.to_string(), true);
    }
    (redact_secrets(value).into_owned(), false)
}

/// Mask a whole environment map for display, entry by entry through
/// [`redact_env_value`] (RAL-324). Output order follows the map's own key
/// order, so a `BTreeMap` input yields alphabetical rows.
#[must_use]
pub fn redact_env_map(
    env: &std::collections::BTreeMap<String, String>,
    secret_names: &std::collections::BTreeSet<String>,
) -> Vec<EnvVarDisplay> {
    env.iter()
        .map(|(name, value)| {
            let (value, redacted) = redact_env_value(name, value, secret_names);
            EnvVarDisplay {
                name: name.clone(),
                value,
                redacted,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_the_ral247_named_secrets() {
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
        ] {
            assert!(is_secret_env_key(key), "{key} should be secret");
        }
    }

    #[test]
    fn heuristic_catches_provider_credentials_generically() {
        for key in [
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GITHUB_TOKEN",
            "PASSWORD",
            "PIPER_SECRET",
            "GOOGLE_CREDENTIALS",
        ] {
            assert!(is_secret_env_key(key), "{key} should be secret");
        }
    }

    #[test]
    fn non_secret_keys_are_left_alone() {
        for key in [
            "PATH",
            "HOME",
            "MONKEY",
            "NODE_ENV",
            "COLUMNS",
            "EDITOR",
            "ANTHROPIC_BASE",
        ] {
            assert!(!is_secret_env_key(key), "{key} should not be secret");
        }
    }

    #[test]
    fn redacts_pwsh_assignment_value() {
        let sentinel = "sk-ant-test-1234";
        let line =
            format!("$env:ANTHROPIC_AUTH_TOKEN = '{sentinel}'; & 'ralphus-runner' 'spec.json'");
        let out = redact_secrets(&line);
        assert!(!out.contains(sentinel), "value leaked: {out}");
        assert!(
            out.contains("$env:ANTHROPIC_AUTH_TOKEN = '[REDACTED]'"),
            "{out}"
        );
        assert!(out.contains("& 'ralphus-runner' 'spec.json'"), "{out}");
    }

    #[test]
    fn redacts_multiple_pwsh_assignments_on_one_line() {
        let line =
            r"$env:ANTHROPIC_AUTH_TOKEN = 'sk-one'; $env:ANTHROPIC_API_KEY = 'sk-two'; & 'x'";
        let out = redact_secrets(line);
        assert!(!out.contains("sk-one"));
        assert!(!out.contains("sk-two"));
        assert_eq!(out.matches("[REDACTED]").count(), 2, "{out}");
    }

    #[test]
    fn redacts_pwsh_value_with_an_embedded_quote() {
        // PowerShell doubles a literal '' inside a single-quoted string.
        let line = r"$env:ANTHROPIC_AUTH_TOKEN = 'abc''def'; & 'x'";
        let out = redact_secrets(line);
        assert!(out.contains("'[REDACTED]'"), "{out}");
        assert!(!out.contains("abc"));
    }

    #[test]
    fn redacts_posix_assignment_value() {
        let sentinel = "sk-ant-test-5678";
        let line = format!("ANTHROPIC_AUTH_TOKEN='{sentinel}' ralphus-runner spec.json");
        let out = redact_secrets(&line);
        assert!(!out.contains(sentinel), "value leaked: {out}");
        assert!(out.contains("ANTHROPIC_AUTH_TOKEN='[REDACTED]'"), "{out}");
        assert!(out.contains("ralphus-runner spec.json"), "{out}");
    }

    #[test]
    fn redacts_unquoted_value() {
        let out = redact_secrets(r"$env:ANTHROPIC_AUTH_TOKEN = sk-ant-unquoted; & 'x'");
        assert!(!out.contains("sk-ant-unquoted"));
        assert!(out.contains("= [REDACTED];"), "{out}");
    }

    #[test]
    fn non_secret_assignment_is_left_untouched() {
        let line = "$env:PATH = 'C:\\Windows'; & 'x'";
        let out = redact_secrets(line);
        assert_eq!(out, line, "non-secret value must not be masked");
    }

    #[test]
    fn redacts_mid_line_assignment_after_other_text() {
        let line = "some pane output here\n$env:ANTHROPIC_AUTH_TOKEN = 'sk-late'; & 'x'\ntrailing";
        let out = redact_secrets(line);
        assert!(!out.contains("sk-late"));
        assert!(out.contains("some pane output here"));
        assert!(out.contains("trailing"));
    }

    #[test]
    fn returns_borrowed_when_nothing_to_redact() {
        let text = "ordinary agent output with no secrets here";
        assert!(matches!(redact_secrets(text), Cow::Borrowed(_)));
    }

    #[test]
    fn leaves_a_value_already_redacted_by_the_registry_alone() {
        // RAL-264's registry scrub runs first and replaces a registered
        // secret's value with `<redacted>`; this pattern-based pass must not
        // then clobber that placeholder with its own `[REDACTED]`.
        let line = "$env:ANTHROPIC_AUTH_TOKEN = '<redacted>'; & 'x'";
        let out = redact_secrets(line);
        assert_eq!(out, line, "already-redacted value must be left alone");
    }

    #[test]
    fn leaves_an_already_anchored_nonsecret_key_alone() {
        // The POSIX scanner must not mask `KEY=value` for a non-secret name
        // even though it contains an '='.
        let line = "NODE_ENV=production node index.js";
        let out = redact_secrets(line);
        assert_eq!(out, line);
    }

    #[test]
    fn https_url_with_inline_password_is_flagged() {
        assert!(https_url_has_embedded_password(
            "https://user:hunter2@example.invalid/team/proj.git"
        ));
    }

    #[test]
    fn https_url_with_bare_username_is_not_flagged() {
        // A token-as-username hosting convention (e.g. some CI systems) has
        // no separate password segment, so nothing is inline to leak.
        assert!(!https_url_has_embedded_password(
            "https://ghp_abc123@example.invalid/team/proj.git"
        ));
    }

    #[test]
    fn plain_https_url_is_not_flagged() {
        assert!(!https_url_has_embedded_password(
            "https://example.invalid/team/proj.git"
        ));
    }

    #[test]
    fn ssh_shorthand_url_is_not_flagged() {
        // `git@host:org/repo.git` has no `://`; its leading `git@` is a fixed
        // service account name, not a credential.
        assert!(!https_url_has_embedded_password(
            "git@example.invalid:team/proj.git"
        ));
    }

    #[test]
    fn ssh_scheme_url_is_not_flagged() {
        // Only http(s) is in scope -- `ssh://` auth is key-based, not a
        // password sitting in the URL text.
        assert!(!https_url_has_embedded_password(
            "ssh://user:pass@example.invalid/team/proj.git"
        ));
    }

    #[test]
    fn redact_url_credentials_masks_the_password_but_keeps_host_and_path() {
        let out = redact_url_credentials("https://user:hunter2@example.invalid/team/proj.git");
        assert_eq!(out, "https://***@example.invalid/team/proj.git");
    }

    #[test]
    fn redact_url_credentials_is_a_noop_without_an_at_sign() {
        let url = "https://example.invalid/team/proj.git";
        assert_eq!(redact_url_credentials(url), url);
    }

    #[test]
    fn redact_url_credentials_is_a_noop_for_ssh_shorthand() {
        let url = "git@example.invalid:team/proj.git";
        assert_eq!(redact_url_credentials(url), url);
    }

    fn names(list: &[&str]) -> std::collections::BTreeSet<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn redact_env_value_masks_a_registered_name() {
        let (value, redacted) = redact_env_value("MY_TOKEN", "s3kr3t", &names(&["MY_TOKEN"]));
        assert_eq!(value, REDACTED);
        assert!(redacted);
    }

    #[test]
    fn redact_env_value_shows_an_unregistered_name_even_when_it_looks_secret() {
        // RAL-324: the Secrets tab's list is the only source of truth --
        // there is deliberately no name heuristic here, so a secret-looking
        // but unregistered variable displays in the clear.
        let (value, redacted) = redact_env_value("STRIPE_API_KEY", "sk-live", &names(&[]));
        assert_eq!(value, "sk-live");
        assert!(!redacted);
    }

    #[test]
    fn redact_env_value_still_scrubs_a_credential_assignment_inside_the_value() {
        let (value, redacted) = redact_env_value(
            "WRAPPER_CMD",
            "sh -c \"ANTHROPIC_API_KEY='sk-abc' run\"",
            &names(&[]),
        );
        assert!(!value.contains("sk-abc"), "{value}");
        assert!(!redacted, "the name itself is not registered");
    }

    #[test]
    fn redact_env_map_masks_only_registered_names_and_keeps_key_order() {
        let env: std::collections::BTreeMap<String, String> =
            [("B_PLAIN", "visible"), ("A_TOKEN", "hidden")]
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
        let rows = redact_env_map(&env, &names(&["A_TOKEN"]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "A_TOKEN");
        assert_eq!(rows[0].value, REDACTED);
        assert!(rows[0].redacted);
        assert_eq!(rows[1].name, "B_PLAIN");
        assert_eq!(rows[1].value, "visible");
        assert!(!rows[1].redacted);
    }
}
