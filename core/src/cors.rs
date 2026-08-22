//! Shared CORS origin policy (RAL-220).
//!
//! Neither `daemon/src/server.rs` nor `librarian/src/server.rs` used to emit
//! any `Access-Control-*` headers, which is not the same as denying
//! cross-origin access: a browser can still fire a "simple" (no-preflight)
//! cross-origin request — e.g. a `POST` whose `Content-Type` is set to
//! `text/plain` even though the body is JSON, sidestepping the preflight a
//! real `application/json` request would trigger — and the server executes
//! it; only *reading* the response is blocked by the browser's same-origin
//! policy. So [`decide`] is consulted before a request is dispatched at all:
//! a disallowed `Origin` must reject the request outright, not just omit the
//! response headers.
//!
//! This module is pure/allocation-light (no file or network I/O) so it stays
//! within `ralphus-core`'s "dependency-light and side-effect-free" charter
//! (see the crate doc comment) — each server owns reading its own
//! `[cors]` config table from disk and wires [`decide`]'s result into its own
//! HTTP loop and header types.

use serde::Deserialize;

/// Config-driven CORS allow-list (`[cors]` table). Both `ralphus-daemon` and
/// `ralphus-librarian` read this independently — the librarian's proxy sits
/// in front of the daemon API, so a browser hitting the librarian's port
/// directly must be gated at the librarian's own boundary too, not just the
/// daemon's (see AGENTS.md's RAL-220 risk list).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CorsConfig {
    /// Additional exact-match allowed origins (`scheme://host[:port]`),
    /// beyond the default same-origin allowance computed from the request's
    /// own `Host` header (see [`decide`]). Deliberately no wildcard support.
    /// A list field: layers are unioned via [`Self::merge`], global entries
    /// first then new per-project ones, de-duplicated and order-preserving —
    /// same semantics as every other list-typed config table in this repo
    /// (e.g. `daemon::config::EnvOverridesConfig`).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

impl CorsConfig {
    /// Layer `self` (global) under `over` (per-project): union the allow-lists.
    #[must_use]
    pub fn merge(self, over: CorsConfig) -> CorsConfig {
        let mut allowed_origins = self.allowed_origins;
        for o in over.allowed_origins {
            if !allowed_origins.contains(&o) {
                allowed_origins.push(o);
            }
        }
        CorsConfig { allowed_origins }
    }
}

/// The on-disk file shape: just the one table this module cares about. Each
/// server's own config module parses the same file for its other tables
/// separately — this mirrors only the `[cors]` slice of that shape so
/// `ralphus-core` doesn't need to know about every other table.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    cors: Option<CorsConfig>,
}

/// Parse a `CorsConfig` from TOML text; the default (empty allow-list) when
/// the `[cors]` table is absent or the text is malformed — a broken config
/// file must never fail loudly, matching every other table in this repo.
#[must_use]
pub fn from_toml_str(s: &str) -> CorsConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .cors
        .unwrap_or_default()
}

/// The outcome of evaluating one request's `Origin` header against the
/// allowed list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsDecision {
    /// No `Origin` header was present — not a cross-origin browser request
    /// (a top-level navigation, or any non-browser client: the CLI, a
    /// remote-machine caller per RAL-185, `curl`, ...). CORS is a
    /// browser-only mechanism, so this passes through untouched with no
    /// added headers.
    NotCrossOrigin,
    /// `Origin` matched the request's own `Host` (the default same-origin
    /// allowance) or an explicitly configured entry in `allowed_origins`.
    /// Carries the exact origin to echo back as `Access-Control-Allow-Origin`
    /// (never a wildcard).
    Allowed(String),
    /// `Origin` was present and matched neither the default same-origin
    /// allowance nor the configured allow-list. The caller must reject the
    /// request outright — not merely omit CORS headers — so a "simple"
    /// (no-preflight) cross-origin request never reaches the handler.
    Denied,
}

/// Decide whether a request carrying `origin` (its `Origin` header, if any)
/// is allowed to cross-origin-access this server.
///
/// `host` is the request's own `Host` header (`<hostname>[:port]`, no
/// scheme) — used to compute the default same-origin allowance
/// (`http://<host>` or `https://<host>`) so a browser hitting the daemon or
/// librarian directly at its own address always works with zero
/// configuration. `allowed_origins` is the config-driven allow-list of
/// additional exact-match origins (e.g. a future remote-hosted board),
/// checked case-insensitively; there is deliberately no wildcard support.
#[must_use]
pub fn decide(
    origin: Option<&str>,
    host: Option<&str>,
    allowed_origins: &[String],
) -> CorsDecision {
    let Some(origin) = origin else {
        return CorsDecision::NotCrossOrigin;
    };
    let same_origin = host.is_some_and(|h| {
        origin.eq_ignore_ascii_case(&format!("http://{h}"))
            || origin.eq_ignore_ascii_case(&format!("https://{h}"))
    });
    if same_origin
        || allowed_origins
            .iter()
            .any(|a| a.eq_ignore_ascii_case(origin))
    {
        CorsDecision::Allowed(origin.to_string())
    } else {
        CorsDecision::Denied
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── decide() ─────────────────────────────────────────────────────────

    #[test]
    fn missing_origin_is_not_cross_origin() {
        assert_eq!(
            decide(None, Some("127.0.0.1:7890"), &[]),
            CorsDecision::NotCrossOrigin
        );
    }

    #[test]
    fn origin_matching_host_over_http_is_allowed() {
        assert_eq!(
            decide(Some("http://127.0.0.1:7890"), Some("127.0.0.1:7890"), &[]),
            CorsDecision::Allowed("http://127.0.0.1:7890".to_string())
        );
    }

    #[test]
    fn origin_matching_host_over_https_is_allowed() {
        // Forward-looking: if the daemon/librarian is ever reverse-proxied
        // behind TLS, the browser's Origin scheme flips to https while Host
        // stays the same bare hostname/port.
        assert_eq!(
            decide(Some("https://127.0.0.1:7890"), Some("127.0.0.1:7890"), &[]),
            CorsDecision::Allowed("https://127.0.0.1:7890".to_string())
        );
    }

    #[test]
    fn mismatched_origin_with_empty_allow_list_is_denied() {
        assert_eq!(
            decide(Some("https://evil.example"), Some("127.0.0.1:7890"), &[]),
            CorsDecision::Denied
        );
    }

    #[test]
    fn origin_in_configured_allow_list_is_allowed() {
        let allowed = vec!["https://board.example.com".to_string()];
        assert_eq!(
            decide(
                Some("https://board.example.com"),
                Some("127.0.0.1:7890"),
                &allowed
            ),
            CorsDecision::Allowed("https://board.example.com".to_string())
        );
    }

    #[test]
    fn allow_list_match_is_case_insensitive() {
        let allowed = vec!["https://Board.Example.com".to_string()];
        assert_eq!(
            decide(
                Some("https://board.example.com"),
                Some("127.0.0.1:7890"),
                &allowed
            ),
            CorsDecision::Allowed("https://board.example.com".to_string())
        );
    }

    #[test]
    fn no_wildcard_support() {
        // A literal "*" configured entry only matches the literal origin
        // string "*" (which no real browser ever sends) -- there is no
        // special-cased wildcard behavior.
        let allowed = vec!["*".to_string()];
        assert_eq!(
            decide(
                Some("https://evil.example"),
                Some("127.0.0.1:7890"),
                &allowed
            ),
            CorsDecision::Denied
        );
    }

    #[test]
    fn absent_host_header_still_allows_configured_origins() {
        let allowed = vec!["https://board.example.com".to_string()];
        assert_eq!(
            decide(Some("https://board.example.com"), None, &allowed),
            CorsDecision::Allowed("https://board.example.com".to_string())
        );
    }

    #[test]
    fn absent_host_header_denies_unconfigured_origin() {
        assert_eq!(
            decide(Some("https://evil.example"), None, &[]),
            CorsDecision::Denied
        );
    }

    // ── CorsConfig parsing / merge ───────────────────────────────────────

    #[test]
    fn cors_config_defaults_when_table_absent() {
        assert_eq!(from_toml_str(""), CorsConfig::default());
    }

    #[test]
    fn cors_config_parses_allowed_origins() {
        let c = from_toml_str(
            "[cors]\nallowed_origins = [\"https://a.example\", \"https://b.example\"]\n",
        );
        assert_eq!(
            c.allowed_origins,
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
    }

    #[test]
    fn cors_config_malformed_toml_is_default() {
        assert_eq!(from_toml_str("not = = valid"), CorsConfig::default());
    }

    #[test]
    fn cors_config_merge_unions_and_dedupes() {
        let global = CorsConfig {
            allowed_origins: vec![
                "https://a.example".to_string(),
                "https://b.example".to_string(),
            ],
        };
        let project = CorsConfig {
            allowed_origins: vec![
                "https://b.example".to_string(),
                "https://c.example".to_string(),
            ],
        };
        let merged = global.merge(project);
        assert_eq!(
            merged.allowed_origins,
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string(),
                "https://c.example".to_string(),
            ]
        );
    }
}
