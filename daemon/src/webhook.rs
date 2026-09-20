//! Webhook delivery verification (Track E). Signature/token verification and
//! payload parsing are pure, testable functions -- no HTTP route lives here
//! (see `server.rs` for the receive route, E2) and no
//! [`config::WebhookConfig`](crate::config::WebhookConfig) reads, so
//! verification stays isolated from the config-loading and route-dispatch
//! code paths it's used from. The one exception is [`Store::record_webhook_delivery`]
//! (E6), a `Store`-touching method colocated here rather than in
//! `store.rs` -- the same "each concern hosts its own `impl Store` methods"
//! pattern already used by `cartographer.rs`/`mailbox.rs`/`pr.rs`.

use crate::store::{Result, Store, now_ms};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Verify a GitHub webhook delivery's `X-Hub-Signature-256` header
/// (`sha256=<hex digest>`) against `secret`, computed as HMAC-SHA256 over
/// the raw request body.
///
/// `raw_body` must be the exact bytes GitHub signed -- call this before any
/// JSON parsing or re-serialization touches the body, since re-encoding can
/// change byte-for-byte content (key order, whitespace) without changing
/// meaning.
pub fn verify_github_signature(secret: &[u8], raw_body: &[u8], signature_header: &str) -> bool {
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Some(expected) = decode_hex(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
        // Only fails for MAC-specific key-length limits HMAC-SHA256 doesn't
        // have (it accepts any key length) -- kept as a checked path anyway
        // since `new_from_slice`'s signature allows it.
        return false;
    };
    mac.update(raw_body);
    mac.verify_slice(&expected).is_ok()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Verify a GitLab webhook delivery's `X-Gitlab-Token` header against the
/// configured secret. Unlike GitHub's HMAC signature, GitLab's token is a
/// plain shared secret sent verbatim, so this is a direct equality check --
/// but it must run in constant time, since a byte-at-a-time timing leak
/// would let an attacker recover the secret through repeated requests.
pub fn verify_gitlab_token(secret: &str, header_token: &str) -> bool {
    if secret.is_empty() {
        // An unset/empty configured secret must never authenticate a
        // request, even one sent with no token header at all -- otherwise
        // a misconfigured hook (secret never set) silently accepts every
        // delivery instead of failing closed.
        return false;
    }
    let secret = secret.as_bytes();
    let header_token = header_token.as_bytes();
    if secret.len() != header_token.len() {
        // `ConstantTimeEq` requires equal-length slices; a length mismatch
        // is not itself sensitive (it's observable from the request size
        // regardless), so a fast-path early return here leaks nothing new.
        return false;
    }
    secret.ct_eq(header_token).into()
}

/// Extract the `(repo, PR/MR number)` hint from a GitHub `pull_request` or
/// GitLab `merge_request` webhook delivery body, if it carries one. `None`
/// covers both a malformed body and any other webhook event type this
/// daemon doesn't (yet) look for a PR/MR in (e.g. GitHub `push`, GitLab
/// `Note Hook`).
///
/// This is a hint only (Track E, E5) -- `route_webhook` uses the returned
/// pair solely to look up an already-recorded PR row via
/// `Store::find_pull_request_by_number`, the same lookup `GET
/// /api/pull-requests` already exposes. The parsed body is never stored;
/// nothing here is treated as authoritative, since an unverified body could
/// claim to be about any PR at all -- only the *matched project* (already
/// established by signature verification before this runs) constrains
/// which `repo` values are even meaningful to look up.
///
/// `repo` is returned in the same shape `Store`'s `repo` column already
/// uses for each forge (see `PullRequestView::repo`'s doc comment): plain
/// `owner/repo` for GitHub, percent-encoded `namespace%2Fproject` for
/// GitLab (`crate::forge`'s own repo-path construction encodes the same
/// way, so this mirrors an established convention rather than inventing a
/// new one).
pub fn extract_pr_hint(kind: crate::forge::ForgeKind, body: &str) -> Option<(String, i64)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    match kind {
        crate::forge::ForgeKind::GitHub => {
            let repo = value
                .get("repository")?
                .get("full_name")?
                .as_str()?
                .to_string();
            let number = value.get("pull_request")?.get("number")?.as_i64()?;
            Some((repo, number))
        }
        crate::forge::ForgeKind::GitLab => {
            let path = value.get("project")?.get("path_with_namespace")?.as_str()?;
            let repo = path.replace('/', "%2F");
            let number = value.get("object_attributes")?.get("iid")?.as_i64()?;
            Some((repo, number))
        }
    }
}

impl Store {
    /// Atomically claim a webhook delivery id for `provider`, returning
    /// `true` only the first time it's seen (Track E, E6). Both GitHub and
    /// GitLab retry an undelivered webhook, and a retry must still be
    /// acknowledged with a `200` but must not be re-processed -- see the
    /// `webhook_deliveries` table's doc comment in `store.rs` for why. A
    /// delivery with no id at all (the header wasn't sent) always returns
    /// `true`: there's nothing to dedupe against, so it's processed as a
    /// first-time delivery every time, same as before E6 existed.
    pub fn record_webhook_delivery(&self, provider: &str, delivery_id: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO webhook_deliveries(provider, delivery_id, received_at_ms) VALUES(?1, ?2, ?3)",
            rusqlite::params![provider, delivery_id, now_ms()],
        )? == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_signature_accepts_correct_hmac() {
        let secret = b"my-webhook-secret";
        let body = br#"{"action":"opened"}"#;
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(body);
        let digest = mac.finalize().into_bytes();
        let header = format!("sha256={}", encode_hex(&digest));
        assert!(verify_github_signature(secret, body, &header));
    }

    #[test]
    fn github_signature_rejects_wrong_secret() {
        let body = br#"{"action":"opened"}"#;
        let mut mac = HmacSha256::new_from_slice(b"correct-secret").unwrap();
        mac.update(body);
        let digest = mac.finalize().into_bytes();
        let header = format!("sha256={}", encode_hex(&digest));
        assert!(!verify_github_signature(b"wrong-secret", body, &header));
    }

    #[test]
    fn github_signature_rejects_tampered_body() {
        let secret = b"my-webhook-secret";
        let original = br#"{"action":"opened"}"#;
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(original);
        let digest = mac.finalize().into_bytes();
        let header = format!("sha256={}", encode_hex(&digest));

        let tampered = br#"{"action":"closed"}"#;
        assert!(!verify_github_signature(secret, tampered, &header));
    }

    #[test]
    fn github_signature_rejects_missing_prefix() {
        assert!(!verify_github_signature(b"secret", b"body", "abcd1234"));
    }

    #[test]
    fn github_signature_rejects_malformed_hex() {
        assert!(!verify_github_signature(
            b"secret",
            b"body",
            "sha256=not-hex-at-all!!"
        ));
        assert!(!verify_github_signature(b"secret", b"body", "sha256=abc"));
    }

    #[test]
    fn github_signature_rejects_empty_header() {
        assert!(!verify_github_signature(b"secret", b"body", ""));
    }

    fn encode_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn gitlab_token_accepts_matching_secret() {
        assert!(verify_gitlab_token("my-shared-secret", "my-shared-secret"));
    }

    #[test]
    fn gitlab_token_rejects_wrong_secret() {
        assert!(!verify_gitlab_token("my-shared-secret", "guessed-secret"));
    }

    #[test]
    fn gitlab_token_rejects_different_length() {
        assert!(!verify_gitlab_token("short", "a-much-longer-guess"));
    }

    #[test]
    fn gitlab_token_rejects_empty_against_configured() {
        assert!(!verify_gitlab_token("my-shared-secret", ""));
    }

    #[test]
    fn gitlab_token_two_empty_strings_do_not_match_by_accident() {
        // Not a real deployment state (an empty configured secret means
        // webhooks were never set up), but the function must not treat
        // "both empty" as a match -- that would let an attacker send no
        // token at all and pass verification against a misconfigured hook.
        assert!(!verify_gitlab_token("", ""));
    }

    #[test]
    fn extract_pr_hint_reads_a_github_pull_request_payload() {
        let body = r#"{
            "action": "synchronize",
            "repository": {"full_name": "acme/widget"},
            "pull_request": {"number": 42}
        }"#;
        let hint = extract_pr_hint(crate::forge::ForgeKind::GitHub, body);
        assert_eq!(hint, Some(("acme/widget".to_string(), 42)));
    }

    #[test]
    fn extract_pr_hint_reads_a_gitlab_merge_request_payload_and_percent_encodes_the_repo() {
        let body = r#"{
            "object_kind": "merge_request",
            "project": {"path_with_namespace": "acme/widget"},
            "object_attributes": {"iid": 7}
        }"#;
        let hint = extract_pr_hint(crate::forge::ForgeKind::GitLab, body);
        assert_eq!(hint, Some(("acme%2Fwidget".to_string(), 7)));
    }

    #[test]
    fn extract_pr_hint_returns_none_for_an_unrelated_github_event() {
        // A GitHub `push` event has no `pull_request` key at all.
        let body = r#"{"ref": "refs/heads/main", "repository": {"full_name": "acme/widget"}}"#;
        assert_eq!(extract_pr_hint(crate::forge::ForgeKind::GitHub, body), None);
    }

    #[test]
    fn extract_pr_hint_returns_none_for_malformed_json() {
        assert_eq!(
            extract_pr_hint(crate::forge::ForgeKind::GitHub, "not json at all"),
            None
        );
    }

    #[test]
    fn record_webhook_delivery_claims_a_delivery_id_exactly_once() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.record_webhook_delivery("github", "delivery-1").unwrap());
        // A retry under the same id must not be claimed a second time.
        assert!(!s.record_webhook_delivery("github", "delivery-1").unwrap());
    }

    #[test]
    fn record_webhook_delivery_keys_on_provider_too() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.record_webhook_delivery("github", "shared-id").unwrap());
        // GitHub and GitLab mint ids from separate namespaces -- the same
        // literal id under a different provider is not a collision.
        assert!(s.record_webhook_delivery("gitlab", "shared-id").unwrap());
    }
}
