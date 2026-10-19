# STRESS-009: protocol edge cases from the malformed-request corpus (low priority)

- **Severity:** low. These are hygiene issues; none crashed or froze the
  daemon. The ones that do are STRESS-001 and STRESS-002.
- **Found by:** `ralphus-stress` fault `malformed_requests`
- **Found at:** `035ef1e70` (staging, 2026-10-18)
- **Status:** open, to triage

| Payload | Observed | Expected | Notes |
|---|---|---|---|
| `unicode_id`: `GET /api/squads/ünïcödé-😀` (raw UTF-8 in the request line) | connection closed with no response | `400` or `404` | tiny_http probably rejects non-ASCII request lines by dropping the socket. Harmless, but clients get no error to act on. |
| `garbage_bytes`: 1 KiB of random bytes | no response for 3 s; socket held open | `400` and close | The per-connection thread waits for a request line that never comes, with no header read timeout. Connected to STRESS-002's missing timeouts. |
| `oversized_body`: 16, 32 and 64 MiB well-framed submits | all `201`, each squad stored | `413` above a sane cap | No body cap at all (STRESS-001). A 64 MiB task file is accepted and persisted, which bloats the DB and every board read that touches it. |
| `hostile_user_header`: `X-Ralphus-User: ../../admin\0` on `GET /api/tasks` | `200` | `400`, or at least not an accepted identity | Check whether reads accept arbitrary, unregistered user names (`current_user`, `server.rs` around `4878`). Probably benign for reads; worth confirming the name is never used in a path. |
| `huge_header_64k`, `two_thousand_headers`, `long_url_100k` | `200` | `431`/`414` | No header or URL limits in tiny_http. This is memory amplification per connection, bounded by bytes actually sent. |
| `toml_no_cells`: a task with no cells | `201` | probably `400` | Validator accepts a task with zero cells. Check this is intended (`core/src/validate.rs`). |

Everything else in the corpus was answered with a sensible `400`, `401` or
`404`: bad versions, smuggling shapes, bad chunked encoding, invalid
JSON/TOML/UTF-8, traversal, SQL-injection-shaped ids, deep JSON nesting, and
negative or non-numeric `Content-Length`. Pipelining and
`Expect: 100-continue` work.
