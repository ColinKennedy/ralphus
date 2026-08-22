//! Bearer-token auth for the daemon's HTTP API (RAL-219), plus the
//! short-lived ticket mechanism that extends it to `/api/events` (RAL-222).
//!
//! A random token is generated once and persisted to `state_dir()/daemon.token`
//! (see [`crate::token_path`]); subsequent daemon startups reuse it rather than
//! rotating it on every restart, so a client that already has a copy of the
//! file (the CLI, the librarian's proxy) keeps working across restarts. Every
//! HTTP route enforces it (see `Daemon::authorized` / `run_http_loop` in
//! `server.rs`) except `/api/events` (SSE — RAL-222 owns that endpoint's auth,
//! since it bypasses `route()` entirely).
//!
//! `/api/events` can't carry the bearer token directly: it's opened by the
//! browser's `EventSource`, which cannot set custom request headers, and a
//! long-lived credential in a URL query string would leak into access logs,
//! proxy logs, and browser history. Instead, [`TicketStore`] mints a
//! short-lived, single-use ticket (via an ordinary bearer-authenticated `POST
//! /api/events/ticket`) that the client then supplies as `?ticket=...` on the
//! `EventSource` URL — a leaked ticket is useless within seconds and can't be
//! replayed.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Random bytes in a generated token, before hex-encoding (32 bytes = 256 bits).
const TOKEN_BYTES: usize = 32;

/// Load the token at `path` if it already holds a non-empty value, otherwise
/// generate a fresh one and persist it. On Unix the file is written with
/// `0600` permissions (owner read/write only) — RAL-230 tracks equivalent
/// Windows ACL hardening as separate, not-yet-built follow-up work.
///
/// # Errors
/// Returns an error if the token file cannot be written.
pub fn load_or_create(path: &Path) -> io::Result<String> {
    if let Ok(existing) = fs::read_to_string(path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let token = generate();
    fs::write(path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

/// Generate a fresh random token, hex-encoded.
fn generate() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut buf).expect("OS randomness source available");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time string comparison, so a wrong guess against the real token
/// doesn't leak timing information about how many leading bytes matched.
#[must_use]
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// How long a minted `/api/events` ticket (RAL-222) stays valid: long enough
/// to cover the round trip from `POST /api/events/ticket` to the client
/// opening its `EventSource`, short enough that a leaked ticket is useless
/// almost immediately.
const TICKET_TTL: Duration = Duration::from_secs(30);

/// A short-lived, single-use ticket store gating the `/api/events` SSE
/// endpoint (RAL-222) — see the module doc comment for why this exists
/// alongside the long-lived bearer token rather than reusing it directly.
/// Cheap to construct; held directly on `Daemon` (not `Arc`-wrapped) since
/// it's only ever touched from the main HTTP accept loop, never from a
/// spawned thread.
#[derive(Default)]
pub struct TicketStore {
    tickets: Mutex<HashMap<String, Instant>>,
    ttl: Option<Duration>,
}

impl TicketStore {
    /// An empty store using the real [`TICKET_TTL`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn ttl(&self) -> Duration {
        self.ttl.unwrap_or(TICKET_TTL)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.tickets.lock().expect("ticket store mutex poisoned")
    }

    /// Mint a fresh single-use ticket, valid for [`TICKET_TTL`]. Opportunistically
    /// prunes expired entries so the map never grows unbounded across a long
    /// daemon uptime.
    pub fn mint(&self) -> String {
        let ticket = generate();
        let ttl = self.ttl();
        let mut tickets = self.lock();
        tickets.retain(|_, issued| issued.elapsed() < ttl);
        tickets.insert(ticket.clone(), Instant::now());
        ticket
    }

    /// Validate `ticket` and consume it if valid, so it can never be replayed
    /// — an unknown, already-used, or expired ticket is rejected.
    #[must_use]
    pub fn consume(&self, ticket: &str) -> bool {
        let ttl = self.ttl();
        match self.lock().remove(ticket) {
            Some(issued) => issued.elapsed() < ttl,
            None => false,
        }
    }

    /// Test-only escape hatch: a store whose tickets expire after `ttl`
    /// instead of the real [`TICKET_TTL`], so expiry can be exercised without
    /// a real 30s sleep.
    #[cfg(test)]
    fn with_ttl(ttl: Duration) -> Self {
        Self {
            tickets: Mutex::new(HashMap::new()),
            ttl: Some(ttl),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_a_64_char_hex_string() {
        let t = generate();
        assert_eq!(t.len(), TOKEN_BYTES * 2);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_is_not_deterministic() {
        assert_ne!(generate(), generate());
    }

    #[test]
    fn load_or_create_persists_a_token_when_none_exists() {
        let dir = tempdir();
        let path = dir.join("daemon.token");
        let token = load_or_create(&path).expect("create token");
        assert_eq!(token.len(), TOKEN_BYTES * 2);
        assert_eq!(fs::read_to_string(&path).expect("read"), token);
    }

    #[test]
    fn load_or_create_reuses_an_existing_token() {
        let dir = tempdir();
        let path = dir.join("daemon.token");
        fs::write(&path, "existing-token-value").expect("seed");
        let token = load_or_create(&path).expect("load token");
        assert_eq!(token, "existing-token-value");
    }

    #[test]
    fn load_or_create_trims_whitespace_from_an_existing_file() {
        let dir = tempdir();
        let path = dir.join("daemon.token");
        fs::write(&path, "existing-token-value\n").expect("seed");
        let token = load_or_create(&path).expect("load token");
        assert_eq!(token, "existing-token-value");
    }

    #[test]
    fn load_or_create_regenerates_when_the_file_is_empty() {
        let dir = tempdir();
        let path = dir.join("daemon.token");
        fs::write(&path, "").expect("seed");
        let token = load_or_create(&path).expect("create token");
        assert_eq!(token.len(), TOKEN_BYTES * 2);
    }

    #[cfg(unix)]
    #[test]
    fn load_or_create_sets_owner_only_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let path = dir.join("daemon.token");
        load_or_create(&path).expect("create token");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn constant_time_eq_matches_equal_strings() {
        assert!(constant_time_eq("abc123", "abc123"));
    }

    #[test]
    fn constant_time_eq_rejects_different_strings() {
        assert!(!constant_time_eq("abc123", "abc124"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn ticket_store_mint_produces_a_64_char_hex_string() {
        let store = TicketStore::new();
        let ticket = store.mint();
        assert_eq!(ticket.len(), TOKEN_BYTES * 2);
        assert!(ticket.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn ticket_store_consumes_a_freshly_minted_ticket() {
        let store = TicketStore::new();
        let ticket = store.mint();
        assert!(store.consume(&ticket));
    }

    #[test]
    fn ticket_store_rejects_replay_of_an_already_consumed_ticket() {
        let store = TicketStore::new();
        let ticket = store.mint();
        assert!(store.consume(&ticket));
        assert!(!store.consume(&ticket));
    }

    #[test]
    fn ticket_store_rejects_an_unknown_ticket() {
        let store = TicketStore::new();
        assert!(!store.consume("never-issued"));
    }

    #[test]
    fn ticket_store_rejects_an_expired_ticket() {
        let store = TicketStore::with_ttl(Duration::from_millis(1));
        let ticket = store.mint();
        std::thread::sleep(Duration::from_millis(20));
        assert!(!store.consume(&ticket));
    }

    /// A throwaway directory under the OS temp dir, unique per call — keeps
    /// these tests off the real `state_dir()` (same idiom as `tmux.rs`'s test
    /// helpers).
    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-token-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }
}
