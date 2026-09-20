//! Webhook delivery verification (Track E). Pure, testable functions only --
//! no HTTP route lives here (see `server.rs` for the receive route, E2)
//! and no [`config::WebhookConfig`](crate::config::WebhookConfig) reads.
//! Signature/token verification is checked before anything about a delivery
//! is trusted, so it is kept isolated from both the config-loading and
//! route-dispatch code paths it's used from.

use hmac::{Hmac, Mac};
use sha2::Sha256;

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
}
