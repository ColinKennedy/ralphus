//! Webhook delivery verification (Track E). Signature/token verification and
//! payload parsing are pure, testable functions -- no HTTP route lives here
//! (see `server.rs` for the receive route, E2) and no
//! [`config::WebhookConfig`](crate::config::WebhookConfig) reads, so
//! verification stays isolated from the config-loading and route-dispatch
//! code paths it's used from. The exceptions are [`Store::record_webhook_delivery`]
//! (E6), the `project_webhooks` bookkeeping methods (E9), and
//! [`Store::record_webhook_shadow_delivery`] (Track F, F1) -- `Store`-touching
//! methods colocated here rather than in `store.rs` -- the same "each
//! concern hosts its own `impl Store` methods" pattern already used by
//! `cartographer.rs`/`mailbox.rs`/`pr.rs`.

use crate::store::{Result, Store, now_ms};
use hmac::{Hmac, Mac};
use rusqlite::OptionalExtension;
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

    /// Record (or replace) which webhook this daemon installed for a
    /// project (Track E, E9) -- called after a successful `install` or
    /// `update`, so a later `update`/removal-cleanup knows which hook id to
    /// act on without the caller supplying it again.
    pub fn record_project_webhook(
        &self,
        project_name: &str,
        provider: &str,
        hook_id: &str,
        daemon_url: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO project_webhooks(project_name, provider, hook_id, daemon_url, installed_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(project_name) DO UPDATE SET
                 provider=excluded.provider,
                 hook_id=excluded.hook_id,
                 daemon_url=excluded.daemon_url,
                 installed_at_ms=excluded.installed_at_ms",
            rusqlite::params![project_name, provider, hook_id, daemon_url, now_ms()],
        )?;
        Ok(())
    }

    /// The webhook this daemon last recorded installing for a project, if
    /// any (Track E, E9).
    pub fn get_project_webhook(&self, project_name: &str) -> Result<Option<ProjectWebhookRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT provider, hook_id, daemon_url, installed_at_ms FROM project_webhooks WHERE project_name=?1",
                rusqlite::params![project_name],
                |r| {
                    Ok(ProjectWebhookRecord {
                        provider: r.get(0)?,
                        hook_id: r.get(1)?,
                        daemon_url: r.get(2)?,
                        installed_at_ms: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Forget the recorded webhook for a project (Track E, E9) -- called
    /// after a successful `uninstall`, or as best-effort cleanup when the
    /// project itself is removed.
    pub fn delete_project_webhook(&self, project_name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM project_webhooks WHERE project_name=?1",
            rusqlite::params![project_name],
        )?;
        Ok(())
    }
}

/// A webhook this daemon previously recorded installing for a project
/// (Track E, E9) -- see `Store::get_project_webhook`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectWebhookRecord {
    pub provider: String,
    pub hook_id: String,
    pub daemon_url: String,
    pub installed_at_ms: i64,
}

/// One recorded shadow-mode webhook delivery (Track F, F1/F2) -- see
/// `Store::record_webhook_shadow_delivery`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowDeliveryRecord {
    pub provider: String,
    pub delivery_id: Option<String>,
    pub project_name: String,
    pub pr_id: Option<String>,
    pub arrived_at_ms: i64,
    /// Snapshot of `guardian_pr_forge_cache.last_checked_at_ms` for the
    /// resolved PR, taken at record time (Track F, F2). `None` means the
    /// poll had never checked this PR as of the delivery's arrival -- the
    /// clearest "the poll would have missed this entirely" signal.
    pub poll_last_checked_at_ms: Option<i64>,
    /// `arrived_at_ms - poll_last_checked_at_ms`: positive means the poll's
    /// last look predates this delivery by that many milliseconds. `None`
    /// under the same condition as `poll_last_checked_at_ms`.
    pub poll_lag_ms: Option<i64>,
}

impl Store {
    /// Record one verified webhook delivery while a project's `[webhook]`
    /// mode is `"shadow"` (Track F, F1) -- purely observational, never
    /// acted on. Snapshots the poll's current knowledge of the resolved PR
    /// at the same time (F2), so the delta between "the webhook just told
    /// us this" and "the poll's last actual look" is captured once, at the
    /// moment it's most meaningful, rather than reconstructed later from
    /// two independently-drifting timestamps.
    pub fn record_webhook_shadow_delivery(
        &self,
        provider: &str,
        delivery_id: Option<&str>,
        project_name: &str,
        pr_id: Option<&str>,
    ) -> Result<ShadowDeliveryRecord> {
        let arrived_at_ms = now_ms();
        let poll_last_checked_at_ms = match pr_id {
            Some(pr_id) => self
                .conn
                .query_row(
                    "SELECT last_checked_at_ms FROM guardian_pr_forge_cache WHERE pr_id=?1",
                    rusqlite::params![pr_id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?,
            None => None,
        };
        let poll_lag_ms = poll_last_checked_at_ms.map(|checked| arrived_at_ms - checked);
        self.conn.execute(
            "INSERT INTO webhook_shadow_deliveries(
                 provider, delivery_id, project_name, pr_id, arrived_at_ms,
                 poll_last_checked_at_ms, poll_lag_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                provider,
                delivery_id,
                project_name,
                pr_id,
                arrived_at_ms,
                poll_last_checked_at_ms,
                poll_lag_ms
            ],
        )?;
        Ok(ShadowDeliveryRecord {
            provider: provider.to_string(),
            delivery_id: delivery_id.map(str::to_string),
            project_name: project_name.to_string(),
            pr_id: pr_id.map(str::to_string),
            arrived_at_ms,
            poll_last_checked_at_ms,
            poll_lag_ms,
        })
    }
}

/// Aggregated shadow-mode delivery history for one project (Track F, F3) --
/// see `Store::webhook_shadow_scorecard`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowScorecard {
    pub total_deliveries: i64,
    /// Deliveries that resolved to a real PR the poll had never checked as
    /// of arrival (`pr_id` present, `poll_last_checked_at_ms` was `None`)
    /// -- the poll would have missed this entirely without the webhook.
    /// Deliberately excludes [`Self::spurious_count`]'s rows: a delivery
    /// with no resolved PR trivially has no poll data either, but that's a
    /// different failure mode ("this delivery wasn't about anything ralphus
    /// tracks") from "the poll hadn't caught up yet", and conflating them
    /// would double-count every spurious delivery as also missed.
    pub missed_count: i64,
    /// Deliveries that verified but resolved to no PR at all (an event
    /// type `extract_pr_hint`, E5, doesn't parse a PR/MR from, or a PR
    /// ralphus never submitted).
    pub spurious_count: i64,
    /// Mean `poll_lag_ms` across deliveries where it was computed (i.e.
    /// excluding `missed_count`'s rows, which have no lag to average).
    /// `None` when there is nothing to average.
    pub avg_lag_ms: Option<f64>,
    pub max_lag_ms: Option<i64>,
    /// Deliveries for a PR whose `arrived_at_ms` is earlier than an
    /// already-recorded delivery for the *same* PR -- a sign either forge
    /// delivered its events out of send order, or two concurrent
    /// connections raced. Computed by walking recorded rows in insertion
    /// order per PR, not by comparing forge-side event sequence numbers
    /// (neither forge's webhook payload carries one this daemon parses).
    pub out_of_order_count: i64,
}

impl Store {
    /// Aggregate a project's `webhook_shadow_deliveries` history into a
    /// scorecard (Track F, F3) -- the actual "should this project's
    /// `[webhook]` mode graduate from `\"shadow\"` to `\"active\"`?"
    /// evidence F1/F2 exist to build.
    pub fn webhook_shadow_scorecard(&self, project_name: &str) -> Result<ShadowScorecard> {
        let (total_deliveries, missed_count, spurious_count, avg_lag_ms, max_lag_ms) = self
            .conn
            .query_row(
                "SELECT
                     COUNT(*),
                     SUM(CASE WHEN pr_id IS NOT NULL AND poll_last_checked_at_ms IS NULL THEN 1 ELSE 0 END),
                     SUM(CASE WHEN pr_id IS NULL THEN 1 ELSE 0 END),
                     AVG(poll_lag_ms),
                     MAX(poll_lag_ms)
                 FROM webhook_shadow_deliveries WHERE project_name=?1",
                rusqlite::params![project_name],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        r.get::<_, Option<f64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                    ))
                },
            )?;

        // Out-of-order detection needs per-PR ordering, which the
        // aggregate query above can't express -- walked separately here
        // rather than forced into one SQL statement.
        let mut stmt = self.conn.prepare(
            "SELECT pr_id, arrived_at_ms FROM webhook_shadow_deliveries
             WHERE project_name=?1 AND pr_id IS NOT NULL ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![project_name], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut last_seen: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        let mut out_of_order_count = 0i64;
        for (pr_id, arrived_at_ms) in rows {
            if let Some(&previous) = last_seen.get(&pr_id) {
                if arrived_at_ms < previous {
                    out_of_order_count += 1;
                }
            }
            last_seen.insert(pr_id, arrived_at_ms);
        }

        Ok(ShadowScorecard {
            total_deliveries,
            missed_count,
            spurious_count,
            avg_lag_ms,
            max_lag_ms,
            out_of_order_count,
        })
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

    #[test]
    fn project_webhook_round_trips_through_record_get_delete() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.get_project_webhook("proj").unwrap(), None);

        s.record_project_webhook("proj", "github", "42", "https://old.example.com")
            .unwrap();
        let record = s.get_project_webhook("proj").unwrap().unwrap();
        assert_eq!(record.provider, "github");
        assert_eq!(record.hook_id, "42");
        assert_eq!(record.daemon_url, "https://old.example.com");

        s.delete_project_webhook("proj").unwrap();
        assert_eq!(s.get_project_webhook("proj").unwrap(), None);
    }

    #[test]
    fn record_project_webhook_replaces_the_prior_row_for_the_same_project() {
        let s = Store::open_in_memory().unwrap();
        s.record_project_webhook("proj", "github", "42", "https://old.example.com")
            .unwrap();
        s.record_project_webhook("proj", "github", "43", "https://new.example.com")
            .unwrap();
        let record = s.get_project_webhook("proj").unwrap().unwrap();
        assert_eq!(record.hook_id, "43");
        assert_eq!(record.daemon_url, "https://new.example.com");
    }

    // ── Track F, F1: shadow-mode delivery recording ──────────────────────

    #[test]
    fn shadow_delivery_round_trips_through_the_table() {
        let s = Store::open_in_memory().unwrap();
        let record = s
            .record_webhook_shadow_delivery("gitlab", None, "proj", None)
            .unwrap();
        assert_eq!(record.provider, "gitlab");
        assert_eq!(record.project_name, "proj");
        assert_eq!(record.pr_id, None);
        let count: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM webhook_shadow_deliveries", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn shadow_delivery_records_a_resolved_pr_and_delivery_id() {
        let s = Store::open_in_memory().unwrap();
        let record = s
            .record_webhook_shadow_delivery("github", Some("d1"), "proj", Some("pr-1"))
            .unwrap();
        assert_eq!(record.delivery_id, Some("d1".to_string()));
        assert_eq!(record.pr_id, Some("pr-1".to_string()));
        assert!(record.arrived_at_ms > 0);
    }

    #[test]
    fn shadow_delivery_allows_multiple_rows_for_the_same_project() {
        let s = Store::open_in_memory().unwrap();
        s.record_webhook_shadow_delivery("github", Some("d1"), "proj", None)
            .unwrap();
        s.record_webhook_shadow_delivery("github", Some("d2"), "proj", None)
            .unwrap();
        let count: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM webhook_shadow_deliveries", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 2);
    }

    // ── Track F, F2: compare against poll-discovered truth ──────────────

    #[test]
    fn shadow_delivery_with_no_pr_has_no_poll_comparison() {
        let s = Store::open_in_memory().unwrap();
        let record = s
            .record_webhook_shadow_delivery("github", Some("d1"), "proj", None)
            .unwrap();
        assert_eq!(record.poll_last_checked_at_ms, None);
        assert_eq!(record.poll_lag_ms, None);
    }

    #[test]
    fn shadow_delivery_with_a_pr_the_poll_never_checked_has_no_poll_comparison() {
        let s = Store::open_in_memory().unwrap();
        let record = s
            .record_webhook_shadow_delivery("github", Some("d1"), "proj", Some("pr-1"))
            .unwrap();
        assert_eq!(record.pr_id, Some("pr-1".to_string()));
        assert_eq!(record.poll_last_checked_at_ms, None);
        assert_eq!(record.poll_lag_ms, None);
    }

    #[test]
    fn shadow_delivery_captures_the_polls_last_known_check_and_lag() {
        let s = Store::open_in_memory().unwrap();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let pr_id = s
            .create_pull_request(
                &gid,
                None,
                "github",
                "acme/widget",
                "review/demo",
                "main",
                "Demo",
                "",
                Some(7),
                None,
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO guardian_pr_forge_cache(pr_id, last_checked_at_ms) VALUES(?1, ?2)",
                rusqlite::params![pr_id, 1_000_i64],
            )
            .unwrap();
        let record = s
            .record_webhook_shadow_delivery("github", Some("d1"), "proj", Some(&pr_id))
            .unwrap();
        assert_eq!(record.poll_last_checked_at_ms, Some(1_000));
        let lag = record.poll_lag_ms.expect("lag must be computed");
        // arrived_at_ms is real wall-clock time (now_ms()), so it's far
        // ahead of the fixed 1_000 fixture -- just assert the lag is
        // positive and consistent with arrived_at_ms - 1_000.
        assert!(lag > 0);
        assert_eq!(lag, record.arrived_at_ms - 1_000);
    }

    // ── Track F, F3: shadow-mode scorecard ───────────────────────────────

    #[test]
    fn scorecard_of_an_empty_project_is_all_zero() {
        let s = Store::open_in_memory().unwrap();
        let card = s.webhook_shadow_scorecard("proj").unwrap();
        assert_eq!(card.total_deliveries, 0);
        assert_eq!(card.missed_count, 0);
        assert_eq!(card.spurious_count, 0);
        assert_eq!(card.avg_lag_ms, None);
        assert_eq!(card.max_lag_ms, None);
        assert_eq!(card.out_of_order_count, 0);
    }

    #[test]
    fn scorecard_counts_missed_and_spurious_deliveries() {
        let s = Store::open_in_memory().unwrap();
        // Missed: a resolved PR the poll never checked.
        s.record_webhook_shadow_delivery("github", Some("d1"), "proj", Some("pr-1"))
            .unwrap();
        // Spurious: verified, but no PR resolved at all.
        s.record_webhook_shadow_delivery("github", Some("d2"), "proj", None)
            .unwrap();
        let card = s.webhook_shadow_scorecard("proj").unwrap();
        assert_eq!(card.total_deliveries, 2);
        assert_eq!(card.missed_count, 1);
        assert_eq!(card.spurious_count, 1);
    }

    #[test]
    fn scorecard_only_counts_the_named_project() {
        let s = Store::open_in_memory().unwrap();
        s.record_webhook_shadow_delivery("github", Some("d1"), "proj-a", None)
            .unwrap();
        s.record_webhook_shadow_delivery("github", Some("d2"), "proj-b", None)
            .unwrap();
        assert_eq!(
            s.webhook_shadow_scorecard("proj-a")
                .unwrap()
                .total_deliveries,
            1
        );
        assert_eq!(
            s.webhook_shadow_scorecard("proj-b")
                .unwrap()
                .total_deliveries,
            1
        );
    }

    #[test]
    fn scorecard_averages_lag_across_deliveries_that_have_one() {
        let s = Store::open_in_memory().unwrap();
        let gid = s.create_guardian("demo", "main", "/repo").unwrap();
        let pr_id = s
            .create_pull_request(
                &gid,
                None,
                "github",
                "acme/widget",
                "review/demo",
                "main",
                "Demo",
                "",
                Some(7),
                None,
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO guardian_pr_forge_cache(pr_id, last_checked_at_ms) VALUES(?1, ?2)",
                rusqlite::params![pr_id, 100_i64],
            )
            .unwrap();
        s.record_webhook_shadow_delivery("github", Some("d1"), "proj", Some(&pr_id))
            .unwrap();
        // A missed delivery has no lag and must not pull the average down
        // to zero -- AVG() over the lag column should skip its NULL.
        s.record_webhook_shadow_delivery("github", Some("d2"), "proj", Some("pr-never-checked"))
            .unwrap();
        let card = s.webhook_shadow_scorecard("proj").unwrap();
        assert_eq!(card.total_deliveries, 2);
        assert_eq!(card.missed_count, 1);
        assert!(card.avg_lag_ms.expect("one delivery has a lag") > 0.0);
    }

    #[test]
    fn scorecard_detects_an_out_of_order_arrival_for_the_same_pr() {
        let s = Store::open_in_memory().unwrap();
        // Two deliveries for the same PR, inserted in order, but the
        // second's arrived_at_ms is BEFORE the first's -- simulated
        // directly via SQL, since real arrived_at_ms is wall-clock time
        // and can't be controlled from the public recording API.
        s.conn
            .execute(
                "INSERT INTO webhook_shadow_deliveries(provider, delivery_id, project_name, pr_id, arrived_at_ms) VALUES('github','d1','proj','pr-1',2000)",
                [],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO webhook_shadow_deliveries(provider, delivery_id, project_name, pr_id, arrived_at_ms) VALUES('github','d2','proj','pr-1',1000)",
                [],
            )
            .unwrap();
        let card = s.webhook_shadow_scorecard("proj").unwrap();
        assert_eq!(card.out_of_order_count, 1);
    }

    #[test]
    fn scorecard_does_not_flag_in_order_arrivals_across_different_prs() {
        let s = Store::open_in_memory().unwrap();
        s.record_webhook_shadow_delivery("github", Some("d1"), "proj", Some("pr-1"))
            .unwrap();
        s.record_webhook_shadow_delivery("github", Some("d2"), "proj", Some("pr-2"))
            .unwrap();
        let card = s.webhook_shadow_scorecard("proj").unwrap();
        assert_eq!(card.out_of_order_count, 0);
    }
}
