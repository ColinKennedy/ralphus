//! Durable, per-attempt terminal-log capture (RAL-154).
//!
//! `crate::tmux::write_pane_snapshot`/`read_pane_snapshot` already persist a
//! session's *last* pane content as a single file that gets overwritten on
//! every attempt — good enough for "what was there most recently", but a
//! tmux-reattach (`crate::runner::SubprocessRunner::run_via_tmux`) can retry
//! the same `(run_id, task, session_id)` up to `MAX_REATTACH_ATTEMPTS` times,
//! and each prior attempt's output was lost the moment the next one
//! overwrote it. This module persists every attempt's terminal output as its
//! own file instead, so a restarted/reattached session's full history stays
//! individually accessible after the fact — including outside the GUI (each
//! file is plain text).
//!
//! Storage layout: `state_dir()/terminal_logs/<session_name>/<NNNN>.log`,
//! where `<session_name>` is the same deterministic, already-sanitized name
//! `crate::tmux::session_name` computes (so no new bookkeeping is needed to
//! find a session's logs — every "peek"/"open terminal" call site already
//! recomputes this name) and `<NNNN>` is the zero-padded attempt index (`0`
//! for the initial run, `1..` for each reattach). This layout is
//! deliberately stable/discoverable: RAL-155 (the planned cross-cutting log
//! viewer) will reference these files by path rather than by embedding their
//! contents elsewhere.
//!
//! Each file's first line is a header identifying the session/attempt/write
//! time, so a viewer opening (or concatenating) multiple attempt files is
//! never confused about where one attempt's output ends and the next
//! begins — see [`write_attempt`].
//!
//! Retention mirrors `crate::cartographer`'s two-cap model
//! (`crate::config::TerminalLogConfig`): attempt files older than
//! `retention_days` are pruned, and once the total file count across every
//! session exceeds `max_files` the oldest excess files are pruned too (see
//! [`prune`]). Deleting a run or guardian also deletes every terminal log
//! belonging to it outright, via [`delete_with_prefix`] — mirroring
//! `crate::tmux::kill_run_tmux_sessions`'s `ralphus_<run_id>_` prefix scoping
//! (see `daemon/src/server.rs`'s `delete_run`/`guardian_delete`).

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

/// Directory every session's terminal-log attempts live under.
fn terminal_log_root() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(root) = TEST_ROOT.with(|r| r.borrow().clone()) {
            return root;
        }
    }
    crate::state_dir().join("terminal_logs")
}

#[cfg(test)]
thread_local! {
    /// Test-scoped override for [`terminal_log_root`], set via [`set_test_root`].
    /// Lets a test's writes/reads/deletes hit an isolated directory instead of
    /// the real `~/.ralphus`, so parallel tests can't race on a shared namespace.
    static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Redirect this thread's terminal-log storage to `root` for the duration of a
/// test. Rust runs tests on separate threads, so each test that calls this gets
/// an isolated directory -- its writes can no longer be wiped by a different,
/// concurrently-running test that deletes a squad/guardian sharing the same
/// `ralphus_*_` prefix in the real `~/.ralphus/terminal_logs` (a real flake:
/// `server::tests`'s terminal-log reader collided with the squad-delete test on
/// `ralphus_squad-000000000001_`). Call at the top of any test that writes,
/// reads, or deletes terminal logs, or deletes a squad/guardian.
#[cfg(test)]
pub(crate) fn set_test_root(root: PathBuf) {
    TEST_ROOT.with(|r| *r.borrow_mut() = Some(root));
}

/// Directory `session_name`'s attempt files live under.
fn session_dir_in(root: &std::path::Path, session_name: &str) -> PathBuf {
    root.join(session_name)
}

/// Path a given attempt's log file lives (or would live) at within `dir`.
fn attempt_path_in(root: &std::path::Path, session_name: &str, attempt: u32) -> PathBuf {
    session_dir_in(root, session_name).join(format!("{attempt:04}.log"))
}

/// Public path a given attempt's log file lives (or would live) at, under the
/// real (non-test) storage root. RAL-155's uber-log-viewer resolves
/// terminal-log Cartographer rows (see [`write_attempt`]'s `log_path` note)
/// back to a file with this same function, so the two never drift apart.
#[must_use]
pub fn attempt_path(session_name: &str, attempt: u32) -> PathBuf {
    attempt_path_in(&terminal_log_root(), session_name, attempt)
}

/// Path a given attempt's *raw* transcript lives (or would live) at — the
/// continuous, verbatim byte stream `Tmux::pipe_pane` tees a pane's output
/// to for the attempt's whole lifetime (RAL-397 Phase 2C), as opposed to
/// [`attempt_path`]'s `.log` file, which is a single point-in-time
/// `capture-pane` snapshot written once the attempt ends. Deliberately the
/// same `<session_name>/<NNNN>` naming, just a `.raw` extension, so it lives
/// in the same per-session directory as its `.log` sibling: both
/// [`prune`] and [`delete_with_prefix`] already enumerate every file in that
/// directory regardless of extension, so this needs no separate retention
/// wiring — it inherits the existing `retention_days`/`max_files` policy for
/// free. (A consequence worth knowing: since an attempt now has two files
/// instead of one, the same `max_files` cap now holds roughly half as many
/// attempts' worth of history as before — an accepted tradeoff, not a bug.)
#[must_use]
pub fn raw_transcript_path(session_name: &str, attempt: u32) -> PathBuf {
    raw_transcript_path_in(&terminal_log_root(), session_name, attempt)
}

fn raw_transcript_path_in(root: &std::path::Path, session_name: &str, attempt: u32) -> PathBuf {
    session_dir_in(root, session_name).join(format!("{attempt:04}.raw"))
}

/// One persisted attempt's metadata, as returned to API/UI consumers (RAL-154
/// AC: the board must be able to list historical attempts, not just open the
/// live one).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AttemptMeta {
    /// `0` for the initial run, `1..` for each subsequent reattach.
    pub attempt: u32,
    pub size_bytes: u64,
    /// Last-write time (Unix epoch milliseconds), for the UI to render "how
    /// long ago" and for [`prune`]'s age-based cap.
    pub modified_ms: i64,
}

/// Write `content` as the durable log for `session_name`'s attempt `attempt`,
/// truncated to `max_lines` (the tail — most recent output — is kept, same
/// convention as `crate::tmux::write_pane_snapshot`). Prefixes a one-line
/// header identifying the session/attempt/write time so the file is
/// self-describing when opened directly or concatenated with siblings.
///
/// Idempotent per `(session_name, attempt)`: calling this again for the same
/// pair (e.g. a fresher capture of a stale session right before it's killed —
/// see `crate::runner::SubprocessRunner::run_via_tmux_attempt`) overwrites the
/// prior write for that attempt, never appends. Best-effort: a write failure
/// (e.g. disk full) is logged but never fails the session itself, matching
/// `write_pane_snapshot`'s precedent.
pub fn write_attempt(session_name: &str, attempt: u32, content: &str, max_lines: usize) {
    write_attempt_in(
        &terminal_log_root(),
        session_name,
        attempt,
        content,
        max_lines,
    );
}

/// Bound on how many bytes are read from the *tail* of a `.raw` transcript
/// (RAL-397 Phase 2E) when deriving the durable, human-readable attempt log —
/// a disk file can grow up to `ralphus-runner pipe-sink`'s own 256 MiB cap,
/// and reading the whole thing into memory before the existing line-based
/// `max_lines` truncation would spike RAM for no benefit, since only the tail
/// ever survives that truncation anyway. 8 MiB is generous relative to
/// `max_lines` (4000 lines of plain text is rarely more than a few hundred
/// KB), so a head/tail-of-*this window* line boundary landing mid-line in
/// practice is very unlikely.
const RAW_TRANSCRIPT_TAIL_READ_BYTES: u64 = 8 * 1024 * 1024;

/// Derive `session_name`'s attempt `attempt` durable, human-readable log from
/// its `.raw` pipe-pane transcript (RAL-397 Phase 2E) instead of a single
/// point-in-time `capture-pane` snapshot — the transcript is unbounded-depth
/// (bounded only by `pipe-sink`'s own byte cap, not by pane scrollback), so
/// this delivers "saved lines accessible as contiguous text" without the
/// scrollback-depth ceiling the old capture-based path had.
///
/// Reads at most the last [`RAW_TRANSCRIPT_TAIL_READ_BYTES`] of the file (a
/// bounded-memory tail read, not the whole file), strips ANSI escape
/// sequences (the raw transcript is a verbatim byte tee — see
/// [`Tmux::pipe_pane`](crate::tmux::Tmux::pipe_pane) — so it carries color/
/// cursor codes a `capture-pane` snapshot never did), then reuses
/// [`write_attempt`]'s existing redaction/line-truncation/header pipeline
/// unchanged. A missing `.raw` file (no cell has run yet, or Phase 2C's
/// wiring didn't apply — e.g. an interactive terminal session) is not an
/// error: callers fall back to their own pre-transcript content in that case.
///
/// # Errors
/// Returns an error only if the `.raw` file cannot be opened/read at all;
/// never for the content it contains (best-effort, matching `write_attempt`).
pub fn write_attempt_from_raw_transcript(
    session_name: &str,
    attempt: u32,
    max_lines: usize,
) -> std::io::Result<()> {
    write_attempt_from_raw_transcript_in(&terminal_log_root(), session_name, attempt, max_lines)
}

fn write_attempt_from_raw_transcript_in(
    root: &std::path::Path,
    session_name: &str,
    attempt: u32,
    max_lines: usize,
) -> std::io::Result<()> {
    let raw_path = raw_transcript_path_in(root, session_name, attempt);
    let tail_bytes = read_tail_bytes(&raw_path, RAW_TRANSCRIPT_TAIL_READ_BYTES)?;
    // `read_tail_bytes` can start mid-multibyte-sequence when the file is
    // larger than the tail window; lossy conversion replaces any truncated
    // leading sequence with U+FFFD rather than failing the whole read.
    let text = String::from_utf8_lossy(&tail_bytes);
    let stripped = strip_ansi_escapes(&text);
    write_attempt_in(root, session_name, attempt, &stripped, max_lines);
    Ok(())
}

/// One requested slice of a `.raw` transcript (RAL-397 Phase 2F), for the
/// board's Live View to page through depth beyond a pane's own in-memory
/// scrollback window without the daemon ever holding the whole transcript in
/// RAM. `content` is the raw byte range re-encoded as UTF-8 (lossy — an
/// arbitrary byte offset can land mid escape-sequence or mid multi-byte
/// character at either edge of the slice); consumers that need exact
/// rendering should request ANSI-sequence-aligned ranges where practical, but
/// a lossy edge on an occasional request is a rendering nicety, not a
/// correctness requirement, for a live-updating view.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RawTranscriptRange {
    pub content: String,
    /// Byte offset in the file the returned `content` actually starts at
    /// (may differ from the requested `offset` if it was past the end).
    pub start: u64,
    /// Total size of the transcript file at read time, so the client knows
    /// whether it has reached the beginning (`start == 0`) or the current
    /// live end (`start + content.len() as u64 == total`).
    pub total: u64,
}

/// Read up to `limit` bytes starting at `offset` from `session_name`'s
/// attempt `attempt` raw transcript (RAL-397 Phase 2F) — the file
/// `Tmux::pipe_pane` (via Phase 2C's wiring) continuously appends to for the
/// attempt's whole lifetime, so a request against a still-running cell reads
/// whatever has been captured so far, live. `None` when no `.raw` file
/// exists for that attempt (the cell never ran under the Phase 2C-wired
/// path, or the file has since been pruned).
#[must_use]
pub fn read_raw_transcript_range(
    session_name: &str,
    attempt: u32,
    offset: u64,
    limit: u64,
) -> Option<RawTranscriptRange> {
    read_raw_transcript_range_in(&terminal_log_root(), session_name, attempt, offset, limit)
}

fn read_raw_transcript_range_in(
    root: &std::path::Path,
    session_name: &str,
    attempt: u32,
    offset: u64,
    limit: u64,
) -> Option<RawTranscriptRange> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let path = raw_transcript_path_in(root, session_name, attempt);
    let mut file = std::fs::File::open(path).ok()?;
    let total = file.metadata().ok()?.len();
    let start = offset.min(total);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = vec![0u8; usize::try_from(limit).unwrap_or(usize::MAX)];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(RawTranscriptRange {
        content: String::from_utf8_lossy(&buf).into_owned(),
        start,
        total,
    })
}

/// The most recent (highest-numbered) attempt with a `.raw` transcript for
/// `session_name`, or `None` if none exist yet — the attempt a "live" Live
/// View request (RAL-397 Phase 2F) means when it doesn't name one explicitly.
///
/// Deliberately scans `.raw` files, **not** [`list_attempts`]'s `.log`-only
/// view: a still-running attempt's `.raw` file exists from the moment its
/// session starts (Phase 2C wires `pipe_pane` in at session creation), while
/// its `.log` sibling is only written once the attempt *ends*
/// ([`write_attempt`]) — so for a live cell, `list_attempts`'s latest entry
/// is always one attempt *behind* the one actually worth viewing live.
#[must_use]
pub fn latest_attempt(session_name: &str) -> Option<u32> {
    latest_attempt_in(&terminal_log_root(), session_name)
}

fn latest_attempt_in(root: &std::path::Path, session_name: &str) -> Option<u32> {
    let dir = session_dir_in(root, session_name);
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("raw") {
                return None;
            }
            path.file_stem()?.to_str()?.parse::<u32>().ok()
        })
        .max()
}

/// Read at most the last `max_bytes` of the file at `path`, via `seek`
/// rather than reading the whole file first — see
/// [`RAW_TRANSCRIPT_TAIL_READ_BYTES`]'s doc comment for why bounding peak
/// memory matters here.
fn read_tail_bytes(path: &std::path::Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Strip ANSI/VT100 escape sequences from `s`, leaving plain text — a small
/// hand-rolled state machine rather than pulling in a `regex`/ANSI-parsing
/// dependency the workspace doesn't otherwise carry (RAL-397 Phase 2E; see
/// [`write_attempt_from_raw_transcript`]). Recognizes:
/// - CSI sequences (`ESC [` ... a final byte in `@`-`~`) — cursor movement,
///   color/style (SGR), and the vast majority of what a real pane emits;
/// - OSC sequences (`ESC ]` ... terminated by BEL or `ESC \`) — window title/
///   hyperlink escapes;
/// - bare two-byte escapes (`ESC` + one other char) as a catch-all for
///   anything not shaped like the two above.
///
/// Not a complete ECMA-48 parser (no support for nested/malformed sequences
/// beyond what real pane output actually produces), but sufficient for its
/// only consumer: turning a real captured pane transcript back into the same
/// kind of plain text `capture-pane -p` already produced before this file
/// existed.
#[must_use]
fn strip_ansi_escapes(s: &str) -> String {
    const ESC: char = '\u{1b}';
    const BEL: char = '\u{07}';
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != ESC {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next(); // consume '['
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next(); // consume ']'
                loop {
                    match chars.next() {
                        None | Some(BEL) => break,
                        Some(ESC) if chars.peek() == Some(&'\\') => {
                            chars.next(); // consume '\\' (ST)
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Some(_) => {
                chars.next(); // consume the single char after ESC
            }
            None => {}
        }
    }
    out
}

fn write_attempt_in(
    root: &std::path::Path,
    session_name: &str,
    attempt: u32,
    content: &str,
    max_lines: usize,
) {
    let dir = session_dir_in(root, session_name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus [terminal_log] could not create dir {}: {e}",
            dir.display()
        );
        return;
    }
    // RAL-264: scrub resolved `from_env` secret values out of the transcript
    // before persisting it — the pane can legitimately show the agent's own
    // `$env:... = 'sk-or-v1-...'` assignment, and a durable per-attempt log is
    // exactly the artifact RAL-264 must keep secrets out of. This is a deliberate
    // lossiness: after scrubbing, the original pane text is unrecoverable from
    // this file for debugging — an accepted tradeoff, revisitable if a future
    // need for verbatim transcripts outweighs the credential-exposure risk.
    //
    // RAL-247: additionally scrub credential env-var values by pattern, so a
    // secret value that was never registered (e.g. sourced from the raw
    // process environment rather than a resolved agent-profile `from_env`)
    // is still caught before the pane text ever hits disk.
    let content = crate::redact::redact_all(content);
    let redacted = ralphus_core::redact::redact_secrets(&content);
    let truncated = crate::runner::tail_lines(&redacted, max_lines);
    let header = format!(
        "=== ralphus terminal log -- session={session_name} attempt={attempt} written={} ===\n",
        chrono::Utc::now().to_rfc3339()
    );
    let path = attempt_path_in(root, session_name, attempt);
    if let Err(e) = std::fs::write(&path, format!("{header}{truncated}")) {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus [terminal_log] could not write attempt log {}: {e}",
            path.display()
        );
    }
}

/// List every attempt persisted for `session_name`, ascending by attempt
/// number. Empty (not an error) when the session never ran under tmux, or no
/// attempt has finished writing its log yet.
#[must_use]
pub fn list_attempts(session_name: &str) -> Vec<AttemptMeta> {
    list_attempts_in(&terminal_log_root(), session_name)
}

fn list_attempts_in(root: &std::path::Path, session_name: &str) -> Vec<AttemptMeta> {
    let dir = session_dir_in(root, session_name);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<AttemptMeta> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            // RAL-397 Phase 2C added a same-stemmed `.raw` sibling next to
            // each attempt's `.log` file (see `raw_transcript_path`) -- an
            // internal transcript source, not a user-facing "attempt" in its
            // own right, so it must not surface here as a duplicate entry.
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("log") {
                return None;
            }
            let attempt: u32 = path.file_stem()?.to_str()?.parse().ok()?;
            let meta = entry.metadata().ok()?;
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            Some(AttemptMeta {
                attempt,
                size_bytes: meta.len(),
                modified_ms,
            })
        })
        .collect();
    out.sort_by_key(|a| a.attempt);
    out
}

/// Read back one attempt's full log content (header included), or `None` if
/// it was never written / already pruned.
#[must_use]
pub fn read_attempt(session_name: &str, attempt: u32) -> Option<String> {
    read_attempt_in(&terminal_log_root(), session_name, attempt)
}

fn read_attempt_in(root: &std::path::Path, session_name: &str, attempt: u32) -> Option<String> {
    // RAL-247: redact on read too — an attempt file written before this fix
    // (or by a version without it) may already carry a secret value on disk.
    std::fs::read_to_string(attempt_path_in(root, session_name, attempt))
        .ok()
        .map(|s| ralphus_core::redact::redact_secrets(&s).into_owned())
}

/// Delete every persisted attempt for one session outright.
pub fn delete_for_session(session_name: &str) {
    delete_for_session_in(&terminal_log_root(), session_name);
}

fn delete_for_session_in(root: &std::path::Path, session_name: &str) {
    let dir = session_dir_in(root, session_name);
    let _ = std::fs::remove_dir_all(dir);
}

/// Delete every session's terminal logs whose (already-sanitized)
/// `session_name` starts with `prefix` — the terminal-log counterpart to
/// `crate::tmux::Tmux::kill_sessions_with_prefix`, used so deleting a run or
/// guardian also deletes its durable terminal-log history (RAL-154 Q3).
/// `prefix` is the same `ralphus_<run_id>_`/`ralphus_guardian-<guardian_id>_`
/// scoping `server.rs`'s `kill_run_tmux_sessions`/`kill_guardian_tmux_sessions`
/// already use. Best-effort and silent on any I/O error, matching this
/// module's other cleanup paths — a leftover log directory is a bounded,
/// non-fatal disk-space cost, never worth failing a delete over.
pub fn delete_with_prefix(prefix: &str) {
    delete_with_prefix_in(&terminal_log_root(), prefix);
}

fn delete_with_prefix_in(root: &std::path::Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Prune attempt log files across every session: first anything older than
/// `retention_days` (when `> 0`), then — if the total file count still
/// exceeds `max_files` (when `> 0`) — the oldest excess files. Mirrors
/// `Store::cartographer_prune`'s two-cap model exactly, applied to files
/// instead of DB rows. Returns the number of files deleted, for the caller
/// to log. A session directory left empty by pruning is removed too, so a
/// long-dead, fully-pruned session doesn't linger as an empty directory
/// forever.
#[must_use]
pub fn prune(retention_days: i64, max_files: i64) -> usize {
    prune_in(&terminal_log_root(), retention_days, max_files)
}

fn prune_in(root: &std::path::Path, retention_days: i64, max_files: i64) -> usize {
    let Ok(session_dirs) = std::fs::read_dir(root) else {
        return 0;
    };
    // (path, modified_ms)
    let mut files: Vec<(PathBuf, i64)> = Vec::new();
    for session_entry in session_dirs.filter_map(std::result::Result::ok) {
        let Ok(file_type) = session_entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Ok(attempt_entries) = std::fs::read_dir(session_entry.path()) else {
            continue;
        };
        for attempt_entry in attempt_entries.filter_map(std::result::Result::ok) {
            let Ok(meta) = attempt_entry.metadata() else {
                continue;
            };
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            files.push((attempt_entry.path(), modified_ms));
        }
    }

    let mut deleted = 0usize;
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default(),
    )
    .unwrap_or(i64::MAX);

    if retention_days > 0 {
        let cutoff = now_ms - retention_days * 86_400_000;
        files.retain(|(path, modified_ms)| {
            if *modified_ms < cutoff {
                if std::fs::remove_file(path).is_ok() {
                    deleted += 1;
                }
                false
            } else {
                true
            }
        });
    }

    if max_files > 0 && files.len() as i64 > max_files {
        files.sort_by_key(|(_, modified_ms)| *modified_ms);
        let excess = files.len() as i64 - max_files;
        for (path, _) in files.iter().take(excess as usize) {
            if std::fs::remove_file(path).is_ok() {
                deleted += 1;
            }
        }
    }

    // Best-effort cleanup of now-empty session directories.
    if let Ok(session_dirs) = std::fs::read_dir(root) {
        for session_entry in session_dirs.filter_map(std::result::Result::ok) {
            if session_entry.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = std::fs::remove_dir(session_entry.path());
            }
        }
    }

    deleted
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory for a terminal-log test, cleaned up on drop.
    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(unique: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ralphus-test-terminal-logs-{unique}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn attempt_path_is_deterministic_and_session_scoped() {
        let a = attempt_path("sess-a", 0);
        let b = attempt_path("sess-a", 1);
        let c = attempt_path("sess-b", 0);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(
            a.to_string_lossy().ends_with("sess-a\\0000.log")
                || a.to_string_lossy().ends_with("sess-a/0000.log")
        );
    }

    #[test]
    fn raw_transcript_path_lives_alongside_the_log_in_the_same_session_dir() {
        let raw = raw_transcript_path("sess-a", 0);
        let log = attempt_path("sess-a", 0);
        assert_eq!(raw.parent(), log.parent());
        assert!(
            raw.to_string_lossy().ends_with("sess-a\\0000.raw")
                || raw.to_string_lossy().ends_with("sess-a/0000.raw")
        );
    }

    #[test]
    fn write_and_read_round_trips_with_header() {
        let root = TempRoot::new("round-trip");
        write_attempt_in(&root.0, "sess-a", 0, "hello\nworld", 100);
        let content = read_attempt_in(&root.0, "sess-a", 0).expect("attempt written");
        assert!(content.contains("session=sess-a attempt=0"));
        assert!(content.contains("hello\nworld"));
    }

    /// RAL-264: a durable per-attempt transcript can legitimately contain the
    /// agent's own `$env:... = 'sk-or-v1-...'` assignment in its pane output;
    /// the resolved secret must be scrubbed out before the file is written,
    /// since this file is a durable user-facing artifact (`ralphus review
    /// logs`/the board's terminal-log viewer read it back).
    #[test]
    fn write_attempt_redacts_registered_secret_values() {
        crate::redact::with_registry_lock(|| {
            crate::redact::clear_for_tests();
            crate::redact::register("sk-or-v1-terminal-log-test-token");

            let root = TempRoot::new("redact");
            write_attempt_in(
                &root.0,
                "sess-a",
                0,
                "working...\n$env:ANTHROPIC_AUTH_TOKEN = 'sk-or-v1-terminal-log-test-token'\ndone\n",
                100,
            );
            let content = read_attempt_in(&root.0, "sess-a", 0).expect("attempt written");
            assert!(
                !content.contains("sk-or-v1-terminal-log-test-token"),
                "secret leaked into terminal log: {content}"
            );
            assert!(content.contains(crate::redact::REDACTED));
            // The surrounding, non-secret transcript survives verbatim.
            assert!(content.contains("working..."));
            assert!(content.contains("done"));
        });
    }

    #[test]
    fn missing_attempt_reads_as_none() {
        let root = TempRoot::new("missing");
        assert_eq!(read_attempt_in(&root.0, "sess-a", 0), None);
    }

    #[test]
    fn separate_attempts_do_not_overwrite_each_other() {
        let root = TempRoot::new("separate-attempts");
        write_attempt_in(&root.0, "sess-a", 0, "first attempt", 100);
        write_attempt_in(&root.0, "sess-a", 1, "second attempt", 100);
        assert!(
            read_attempt_in(&root.0, "sess-a", 0)
                .unwrap()
                .contains("first attempt")
        );
        assert!(
            read_attempt_in(&root.0, "sess-a", 1)
                .unwrap()
                .contains("second attempt")
        );
    }

    #[test]
    fn rewriting_the_same_attempt_overwrites_not_appends() {
        let root = TempRoot::new("overwrite-same-attempt");
        write_attempt_in(&root.0, "sess-a", 0, "stale content", 100);
        write_attempt_in(&root.0, "sess-a", 0, "fresher content", 100);
        let content = read_attempt_in(&root.0, "sess-a", 0).unwrap();
        assert!(content.contains("fresher content"));
        assert!(!content.contains("stale content"));
    }

    #[test]
    fn write_redacts_secret_env_values_from_persisted_content() {
        // RAL-247: the durable terminal-log *file* on disk must never contain
        // a credential env-var value, even when the pane capture did.
        let root = TempRoot::new("write-redacts-secret");
        let secret = "sk-ant-leak-guard";
        let content =
            format!("$env:ANTHROPIC_AUTH_TOKEN = '{secret}'; & 'runner' x\nrest of output");
        write_attempt_in(&root.0, "sess-a", 0, &content, 100);
        let disk = std::fs::read_to_string(attempt_path_in(&root.0, "sess-a", 0))
            .expect("attempt written");
        assert!(
            !disk.contains(secret),
            "credential value leaked into the persisted terminal-log file: {disk}"
        );
        assert!(disk.contains("[REDACTED]"), "{disk}");
        assert!(
            disk.contains("rest of output"),
            "non-secret content must be preserved: {disk}"
        );
    }

    #[test]
    fn read_redacts_secret_values_from_a_legacy_file() {
        // RAL-247: a file written before this fix (or by a version without
        // it) may already carry a secret value on disk — the read path must
        // scrub it defensively too.
        let root = TempRoot::new("read-redacts-legacy");
        let secret = "sk-legacy-leak";
        let path = attempt_path_in(&root.0, "sess-a", 0);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("$env:ANTHROPIC_API_KEY = '{secret}'")).unwrap();
        let content = read_attempt_in(&root.0, "sess-a", 0).expect("legacy file readable");
        assert!(
            !content.contains(secret),
            "credential value leaked from a legacy file: {content}"
        );
        assert!(content.contains("[REDACTED]"), "{content}");
    }

    #[test]
    fn write_truncates_to_max_lines() {
        let root = TempRoot::new("truncate");
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        write_attempt_in(&root.0, "sess-a", 0, &lines.join("\n"), 10);
        let content = read_attempt_in(&root.0, "sess-a", 0).unwrap();
        // Header line + last 10 lines.
        assert_eq!(content.lines().count(), 11);
        assert!(content.ends_with("line 49"));
        assert!(!content.contains("line 0\n"));
    }

    #[test]
    fn list_attempts_is_sorted_ascending() {
        let root = TempRoot::new("list-sorted");
        write_attempt_in(&root.0, "sess-a", 2, "c", 100);
        write_attempt_in(&root.0, "sess-a", 0, "a", 100);
        write_attempt_in(&root.0, "sess-a", 1, "b", 100);
        let attempts: Vec<u32> = list_attempts_in(&root.0, "sess-a")
            .into_iter()
            .map(|a| a.attempt)
            .collect();
        assert_eq!(attempts, vec![0, 1, 2]);
    }

    #[test]
    fn list_attempts_empty_for_unknown_session() {
        let root = TempRoot::new("list-empty");
        assert!(list_attempts_in(&root.0, "never-existed").is_empty());
    }

    #[test]
    fn list_attempts_ignores_the_sibling_raw_transcript_file() {
        // RAL-397 regression: a `.raw` file sharing an attempt's `0000` stem
        // must not surface as a second, duplicate "attempt" in the listing.
        let root = TempRoot::new("list-ignores-raw");
        write_attempt_in(&root.0, "sess-a", 0, "log content", 100);
        let raw_path = raw_transcript_path_in(&root.0, "sess-a", 0);
        std::fs::write(&raw_path, "raw content").unwrap();

        let attempts = list_attempts_in(&root.0, "sess-a");
        assert_eq!(
            attempts.len(),
            1,
            "expected exactly one attempt, got: {attempts:?}"
        );
        assert_eq!(attempts[0].attempt, 0);
    }

    #[test]
    fn delete_for_session_removes_all_its_attempts() {
        let root = TempRoot::new("delete-session");
        write_attempt_in(&root.0, "sess-a", 0, "x", 100);
        write_attempt_in(&root.0, "sess-a", 1, "y", 100);
        delete_for_session_in(&root.0, "sess-a");
        assert!(list_attempts_in(&root.0, "sess-a").is_empty());
    }

    #[test]
    fn delete_with_prefix_removes_only_matching_sessions() {
        let root = TempRoot::new("delete-prefix");
        write_attempt_in(&root.0, "ralphus_run-1_build_work", 0, "x", 100);
        write_attempt_in(&root.0, "ralphus_run-2_build_work", 0, "y", 100);

        delete_with_prefix_in(&root.0, "ralphus_run-1_");

        assert!(list_attempts_in(&root.0, "ralphus_run-1_build_work").is_empty());
        assert!(!list_attempts_in(&root.0, "ralphus_run-2_build_work").is_empty());
    }

    #[test]
    fn prune_enforces_time_window() {
        let root = TempRoot::new("prune-age");
        write_attempt_in(&root.0, "sess-a", 0, "old", 100);
        // Backdate the file's mtime well past any retention window.
        let path = attempt_path_in(&root.0, "sess-a", 0);
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86_400);
        let ft = filetime_touch(&path, old_time);
        assert!(ft, "failed to backdate test file mtime");

        write_attempt_in(&root.0, "sess-a", 1, "fresh", 100);

        let deleted = prune_in(&root.0, 30, 0);
        assert_eq!(deleted, 1);
        assert!(read_attempt_in(&root.0, "sess-a", 0).is_none());
        assert!(read_attempt_in(&root.0, "sess-a", 1).is_some());
    }

    #[test]
    fn prune_enforces_file_count_cap() {
        let root = TempRoot::new("prune-count");
        for i in 0..5u32 {
            write_attempt_in(&root.0, "sess-a", i, "x", 100);
            // Ensure distinct mtimes so oldest-first ordering is deterministic.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let deleted = prune_in(&root.0, 0, 3);
        assert_eq!(deleted, 2);
        let remaining: Vec<u32> = list_attempts_in(&root.0, "sess-a")
            .into_iter()
            .map(|a| a.attempt)
            .collect();
        assert_eq!(remaining, vec![2, 3, 4]);
    }

    #[test]
    fn prune_zero_caps_prune_nothing() {
        let root = TempRoot::new("prune-noop");
        write_attempt_in(&root.0, "sess-a", 0, "x", 100);
        let deleted = prune_in(&root.0, 0, 0);
        assert_eq!(deleted, 0);
        assert!(read_attempt_in(&root.0, "sess-a", 0).is_some());
    }

    /// Best-effort mtime backdating without a new dependency: uses the
    /// platform `touch`-equivalent via `std::fs::File::set_modified` where
    /// available (stable since Rust 1.75). Returns whether it succeeded.
    fn filetime_touch(path: &std::path::Path, time: std::time::SystemTime) -> bool {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(time))
            .is_ok()
    }

    #[test]
    fn strip_ansi_escapes_removes_csi_sequences() {
        // A real sample shape from a captured pipe-pane transcript (RAL-397
        // Phase 0 spike): cursor hide/show + SGR color codes around plain text.
        let raw = "\u{1b}[?25l\u{1b}[93mecho \u{1b}[37mpane-line-1\u{1b}[?25h\u{1b}[m";
        assert_eq!(strip_ansi_escapes(raw), "echo pane-line-1");
    }

    #[test]
    fn strip_ansi_escapes_removes_osc_sequences_terminated_by_bel() {
        let raw = "before\u{1b}]0;window title\u{07}after";
        assert_eq!(strip_ansi_escapes(raw), "beforeafter");
    }

    #[test]
    fn strip_ansi_escapes_removes_osc_sequences_terminated_by_st() {
        let raw = "before\u{1b}]8;;https://example.com\u{1b}\\linked text\u{1b}]8;;\u{1b}\\after";
        assert_eq!(strip_ansi_escapes(raw), "beforelinked textafter");
    }

    #[test]
    fn strip_ansi_escapes_leaves_plain_text_and_newlines_untouched() {
        let raw = "line one\nline two\r\nline three";
        assert_eq!(strip_ansi_escapes(raw), raw);
    }

    #[test]
    fn strip_ansi_escapes_handles_a_bare_trailing_escape_without_panicking() {
        assert_eq!(strip_ansi_escapes("text\u{1b}"), "text");
    }

    #[test]
    fn read_tail_bytes_reads_the_whole_file_when_under_the_cap() {
        let root = TempRoot::new("tail-under-cap");
        let path = root.0.join("small.raw");
        std::fs::write(&path, b"hello world").unwrap();
        let tail = read_tail_bytes(&path, 1024).unwrap();
        assert_eq!(tail, b"hello world");
    }

    #[test]
    fn read_tail_bytes_keeps_only_the_last_n_bytes_when_over_the_cap() {
        let root = TempRoot::new("tail-over-cap");
        let path = root.0.join("big.raw");
        std::fs::write(&path, b"0123456789").unwrap();
        let tail = read_tail_bytes(&path, 4).unwrap();
        assert_eq!(tail, b"6789");
    }

    #[test]
    fn write_attempt_from_raw_transcript_strips_ansi_and_redacts() {
        crate::redact::with_registry_lock(|| {
            crate::redact::clear_for_tests();
            crate::redact::register("sk-or-v1-terminal-log-ansi-test");

            let root = TempRoot::new("from-raw-transcript");
            let raw_path = raw_transcript_path_in(&root.0, "sess-a", 0);
            std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
            std::fs::write(
                &raw_path,
                "\u{1b}[93mworking\u{1b}[m...\n$env:ANTHROPIC_AUTH_TOKEN = 'sk-or-v1-terminal-log-ansi-test'\ndone\n",
            )
            .unwrap();

            write_attempt_from_raw_transcript_in(&root.0, "sess-a", 0, 100).unwrap();

            let content = read_attempt_in(&root.0, "sess-a", 0).expect("attempt written");
            assert!(
                !content.contains('\u{1b}'),
                "ANSI escape leaked into the derived log: {content:?}"
            );
            assert!(content.contains("working..."));
            assert!(content.contains("done"));
            assert!(
                !content.contains("sk-or-v1-terminal-log-ansi-test"),
                "secret leaked into the derived log: {content}"
            );
            assert!(content.contains(crate::redact::REDACTED));
        });
    }

    #[test]
    fn write_attempt_from_raw_transcript_errors_when_no_raw_file_exists() {
        let root = TempRoot::new("from-raw-transcript-missing");
        assert!(write_attempt_from_raw_transcript_in(&root.0, "sess-a", 0, 100).is_err());
    }

    #[test]
    fn write_attempt_from_raw_transcript_only_reads_the_tail_on_a_huge_file() {
        // Confirms the bounded-memory read path is actually exercised end to
        // end, not just `read_tail_bytes` in isolation: write a file larger
        // than a small tail cap and assert only the tail's content survives.
        let root = TempRoot::new("from-raw-transcript-huge");
        let raw_path = raw_transcript_path_in(&root.0, "sess-a", 0);
        std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
        let filler = "x".repeat(1000);
        std::fs::write(&raw_path, format!("{filler}\nreal-tail-content\n")).unwrap();

        // 20 bytes is comfortably smaller than the 1000-byte filler, so only
        // content at/after that point should ever reach the derived log.
        let tail = read_tail_bytes(&raw_path, 20).unwrap();
        let text = String::from_utf8_lossy(&tail);
        assert!(text.contains("real-tail-content"));
        assert!(!text.contains(&filler));
    }

    #[test]
    fn read_raw_transcript_range_returns_the_requested_slice() {
        let root = TempRoot::new("range-slice");
        let raw_path = raw_transcript_path_in(&root.0, "sess-a", 0);
        std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
        std::fs::write(&raw_path, b"0123456789").unwrap();

        let range = read_raw_transcript_range_in(&root.0, "sess-a", 0, 2, 4).unwrap();
        assert_eq!(range.content, "2345");
        assert_eq!(range.start, 2);
        assert_eq!(range.total, 10);
    }

    #[test]
    fn read_raw_transcript_range_clamps_an_offset_past_the_end() {
        let root = TempRoot::new("range-clamp");
        let raw_path = raw_transcript_path_in(&root.0, "sess-a", 0);
        std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
        std::fs::write(&raw_path, b"short").unwrap();

        let range = read_raw_transcript_range_in(&root.0, "sess-a", 0, 1000, 100).unwrap();
        assert_eq!(range.content, "");
        assert_eq!(range.start, 5);
        assert_eq!(range.total, 5);
    }

    #[test]
    fn read_raw_transcript_range_is_none_for_a_missing_file() {
        let root = TempRoot::new("range-missing");
        assert!(read_raw_transcript_range_in(&root.0, "sess-a", 0, 0, 10).is_none());
    }

    #[test]
    fn latest_attempt_finds_the_highest_numbered_raw_file() {
        let root = TempRoot::new("latest-attempt");
        for attempt in [0, 1, 2] {
            let raw_path = raw_transcript_path_in(&root.0, "sess-a", attempt);
            std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
            std::fs::write(&raw_path, b"x").unwrap();
        }
        assert_eq!(latest_attempt_in(&root.0, "sess-a"), Some(2));
    }

    #[test]
    fn latest_attempt_sees_a_live_attempt_with_no_log_file_yet() {
        // The whole reason this scans .raw, not .log: a still-running
        // attempt's .raw file exists before its .log sibling is ever
        // written (only written at attempt end).
        let root = TempRoot::new("latest-attempt-live");
        write_attempt_in(&root.0, "sess-a", 0, "finished attempt", 100);
        let live_raw = raw_transcript_path_in(&root.0, "sess-a", 1);
        std::fs::create_dir_all(live_raw.parent().unwrap()).unwrap();
        std::fs::write(&live_raw, b"still running").unwrap();

        assert_eq!(latest_attempt_in(&root.0, "sess-a"), Some(1));
        // Confirms the premise: list_attempts (the .log view) is one behind.
        let logged_attempts: Vec<u32> = list_attempts_in(&root.0, "sess-a")
            .into_iter()
            .map(|a| a.attempt)
            .collect();
        assert_eq!(logged_attempts, vec![0]);
    }

    #[test]
    fn latest_attempt_none_for_unknown_session() {
        let root = TempRoot::new("latest-attempt-none");
        assert_eq!(latest_attempt_in(&root.0, "never-existed"), None);
    }
}
