//! Streaming client for the daemon's `GET /api/events` SSE endpoint
//! (RAL-222, Track C / C2) -- the CLI's counterpart to the board's
//! `connectEventStream` (`librarian/assets/board/70-sse.js`). Until this
//! existed, nothing outside a browser ever consumed the daemon's push
//! channel: `ralphus listen`/`submit --wait` (and their MCP twins) each ran
//! their own fixed-interval poll loop instead.
//!
//! Mints a short-lived, single-use ticket via the existing (bearer-token
//! authenticated) `POST /api/events/ticket` route -- `/api/events` itself
//! accepts no other credential (see `daemon/src/token.rs`'s module doc for
//! why) -- then opens a long-lived `GET` and parses the plain-text SSE
//! framing `daemon/src/server.rs`'s `serve_events_stream` writes:
//! `event: {kind}\ndata: {json}\n\n` per event, a `: heartbeat\n\n` comment
//! line roughly every 15s otherwise.

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use crate::client::{DaemonClient, DaemonError};

/// One event delivered over `/api/events` -- `kind` is `"squad"`, `"guardian"`,
/// or `"other"` (see `EventKind` in `daemon/src/events.rs`), and `row` is the
/// pushed Cartographer row itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonEvent {
    pub kind: String,
    pub row: serde_json::Value,
}

/// How long a single read on the underlying connection may go without any
/// bytes at all (not just a named event -- a heartbeat comment line counts)
/// before it's treated as dead. Set well above [`Self`]'s heartbeat cadence
/// (15s, `daemon/src/server.rs`'s `SSE_HEARTBEAT`) so two back-to-back missed
/// heartbeats -- not one -- are what trip it; a single slow tick over an
/// otherwise-healthy connection must not cause a spurious reconnect.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// A live `/api/events` connection. Blocks on [`Self::next_event`] (or via
/// its `Iterator` impl) until the next named event arrives, the connection
/// is closed by the daemon, or it goes idle past [`READ_IDLE_TIMEOUT`] --
/// callers that want to keep listening past any of those should reconnect
/// (mint a fresh ticket, [`Self::connect`] again) rather than treating any
/// of them as fatal; the board's own `connectEventStream` does the same.
pub struct EventStream {
    reader: BufReader<Box<dyn Read + Send>>,
}

impl EventStream {
    /// Mints a ticket and opens the stream. The ticket travels as
    /// `?ticket=...` because `/api/events` bypasses the daemon's normal
    /// bearer-token gate entirely (RAL-222) -- but this is a plain `ureq`
    /// request, not a browser `EventSource`, so the `Authorization` header
    /// still goes out too, exactly like every other `DaemonClient` call;
    /// the daemon simply ignores it on this one route.
    ///
    /// # Errors
    /// Returns an error if the ticket mint fails, the daemon is
    /// unreachable, or the connection doesn't come back with a `200`.
    pub fn connect(client: &DaemonClient) -> Result<Self, DaemonError> {
        let ticket = client.post("/api/events/ticket", None)?;
        let ticket = ticket["ticket"].as_str().ok_or_else(|| DaemonError {
            message: "daemon did not return an events ticket".to_string(),
            status_code: None,
        })?;
        let url = format!("{}?ticket={}", client.url("/api/events"), urlencode(ticket));
        // `timeout_connect`/`timeout_read` are `AgentBuilder`-only (not
        // available on a per-request `ureq::Request`), and deliberately
        // *not* paired with `.timeout()` (the whole-call deadline) -- ureq
        // documents that `.timeout()` takes precedence over `timeout_read`
        // when both are set, which would silently cap this long-lived
        // connection at a fixed wall-clock lifetime regardless of how
        // healthy it stayed.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(READ_IDLE_TIMEOUT)
            .build();
        let req = client.authorize(agent.get(&url));
        let resp = req.call().map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                DaemonError {
                    message: if body.is_empty() {
                        format!("HTTP {code}")
                    } else {
                        body
                    },
                    status_code: Some(code),
                }
            }
            other => DaemonError {
                message: format!("could not reach daemon: {other}"),
                status_code: None,
            },
        })?;
        Ok(Self {
            reader: BufReader::new(resp.into_reader()),
        })
    }

    /// Blocks for the next named event, skipping heartbeat comment lines
    /// silently. `Ok(None)` means the daemon closed the connection cleanly
    /// (end of stream); an idle/dropped connection surfaces as `Err`
    /// instead, via the same read that would otherwise have returned the
    /// next line.
    ///
    /// # Errors
    /// Returns an error if the connection drops or goes idle past
    /// [`READ_IDLE_TIMEOUT`].
    pub fn next_event(&mut self) -> Result<Option<DaemonEvent>, DaemonError> {
        loop {
            let mut kind: Option<String> = None;
            let mut data: Option<String> = None;
            loop {
                let mut line = String::new();
                let n = self.reader.read_line(&mut line).map_err(|e| DaemonError {
                    message: format!("event stream read failed: {e}"),
                    status_code: None,
                })?;
                if n == 0 {
                    // Clean EOF. Mid-record (kind/data already seen) never
                    // happens in practice -- the daemon always finishes a
                    // record with its blank line before closing -- but if it
                    // ever did, a half-built record is still discarded here
                    // rather than surfaced as a malformed event.
                    return Ok(None);
                }
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    break; // end of this SSE record
                }
                if let Some(rest) = line.strip_prefix("event: ") {
                    kind = Some(rest.to_string());
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = Some(rest.to_string());
                }
                // Anything else (a `: heartbeat` comment, a field this
                // client doesn't know about) is ignored, not an error --
                // matching the board's own `catch { /* malformed/heartbeat
                // -- ignore */ }`.
            }
            if let (Some(kind), Some(data)) = (kind, data) {
                let row = serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
                return Ok(Some(DaemonEvent { kind, row }));
            }
            // A heartbeat-only record (or one with neither field for some
            // other reason) -- loop for the next record instead of
            // returning nothing, since the stream itself is still alive.
        }
    }
}

impl Iterator for EventStream {
    type Item = Result<DaemonEvent, DaemonError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_event() {
            Ok(Some(event)) => Some(Ok(event)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Minimal percent-encoding for a ticket value in a query string -- tickets
/// are hex-alphanumeric (see `crate::token`'s ticket generation on the
/// daemon side), so this only needs to be correct, not fast or general.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_leaves_alphanumeric_and_unreserved_chars_alone() {
        assert_eq!(urlencode("abcXYZ019-_.~"), "abcXYZ019-_.~");
    }

    #[test]
    fn urlencode_percent_encodes_everything_else() {
        assert_eq!(urlencode("a b+c/d"), "a%20b%2Bc%2Fd");
    }

    /// Feeds `next_event` a hand-built SSE byte stream via an in-memory
    /// reader, bypassing `connect`'s real HTTP call entirely -- this is the
    /// parsing logic's only seam that doesn't need a live daemon.
    fn stream_from(bytes: &'static [u8]) -> EventStream {
        EventStream {
            reader: BufReader::new(Box::new(bytes) as Box<dyn Read + Send>),
        }
    }

    #[test]
    fn parses_one_named_event() {
        let mut s = stream_from(b"event: squad\ndata: {\"id\":1}\n\n");
        let event = s.next_event().unwrap().unwrap();
        assert_eq!(event.kind, "squad");
        assert_eq!(event.row, serde_json::json!({"id": 1}));
    }

    #[test]
    fn parses_multiple_events_in_sequence() {
        let mut s =
            stream_from(b"event: squad\ndata: {\"id\":1}\n\nevent: guardian\ndata: {\"id\":2}\n\n");
        assert_eq!(s.next_event().unwrap().unwrap().kind, "squad");
        assert_eq!(s.next_event().unwrap().unwrap().kind, "guardian");
    }

    #[test]
    fn skips_heartbeat_comment_lines_and_keeps_reading() {
        let mut s = stream_from(b": heartbeat\n\nevent: other\ndata: {}\n\n");
        let event = s.next_event().unwrap().unwrap();
        assert_eq!(event.kind, "other");
    }

    #[test]
    fn returns_none_on_clean_eof() {
        let mut s = stream_from(b"");
        assert_eq!(s.next_event().unwrap(), None);
    }

    #[test]
    fn returns_none_after_the_last_complete_record_on_eof() {
        let mut s = stream_from(b"event: squad\ndata: {\"id\":1}\n\n");
        assert!(s.next_event().unwrap().is_some());
        assert_eq!(s.next_event().unwrap(), None);
    }

    #[test]
    fn malformed_json_payload_yields_a_null_row_not_an_error() {
        let mut s = stream_from(b"event: squad\ndata: not json\n\n");
        let event = s.next_event().unwrap().unwrap();
        assert_eq!(event.row, serde_json::Value::Null);
    }

    #[test]
    fn iterator_impl_yields_events_then_stops_at_eof() {
        let s = stream_from(b"event: squad\ndata: {}\n\n");
        let events: Vec<_> = s.collect();
        assert_eq!(events.len(), 1);
        assert!(events[0].is_ok());
    }
}
