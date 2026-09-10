//! Thin wrapper around the `tmux` binary — the daemon's single choke point
//! for every tmux invocation (RAL-102).
//!
//! Agent-kind session/verify runs (and, transitively, Guardian merge/resolver
//! sessions — they share [`crate::runner::SubprocessRunner`]) execute inside a
//! detached tmux session instead of as a raw child process. That makes a live,
//! pollable "peek" view possible (`capture-pane`) and lets a user attach an
//! interactive terminal to the same session, while the daemon still gets a
//! well-formed result back over a file-based side channel (see
//! `crate::runner::SubprocessRunner::run_via_tmux`).
//!
//! Binary resolution priority (RAL-102 Q4): an explicit [`TMUX_CMD_ENV`]
//! override, then whatever `tmux` resolves to on `PATH`, then an embedded
//! fallback binary (see [`embedded`] — Windows-only today, gated behind the
//! `embedded-tmux` feature since no verified binary is bundled yet).
//!
//! ## Process-tree confinement on cancel (RAL-321, Windows-only)
//!
//! A cell's real OS process tree is `tmux.exe` (server) → the pane's
//! persistent shell → the injected `ralphus-runner.exe` → the agent CLI or
//! `cmd /C <command>` → further children. [`Tmux::kill_session`] used to
//! only free the *name* from tmux's own session table
//! (`force_kill_tmux_processes` catching, at best, the top `tmux.exe` box) —
//! everything underneath kept running. [`new_detached_session_with_command`]
//! now confines the `new-session` client to a Windows Job Object
//! (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) at spawn time, the same shape
//! `daemon/src/proof.rs`'s `ProcessTree` uses for Guardian check gates; the
//! psmux server it starts (and everything the server later spawns into the
//! pane) inherits that job membership automatically, since Windows adds any
//! child of a job member to the same job unless the parent explicitly
//! requests `CREATE_BREAKAWAY_FROM_JOB` (nothing here does). `kill_session`
//! drops the registered job, killing the whole tree in one shot.
//!
//! This only works on Windows: real (POSIX) tmux detaches its server via
//! `setsid()`, which breaks simple process/job-membership inheritance, so
//! there is no equivalent confinement on Unix — a cancelled cell's tree can
//! still leak there. [`Tmux::kill_session`]'s existing
//! `force_kill_tmux_processes` fallback (a literal `tmux.exe`-name sweep)
//! remains as a last resort for sessions the job either wasn't attached to
//! (confinement failed at spawn) or predate this fix.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `history-limit` set on every cell pane (RAL-397 stopgap). This is a
/// per-pane scrollback ceiling, and each scrollback line at the pane's `-x
/// 500` width costs a fixed amount regardless of content on the Windows
/// tmux-alternative build this project targets (psmux stores every
/// scrollback row as a dense 500-cell vector) — so the previous value of
/// `200000` set a ~4.4 GB *per pane* memory ceiling, and an output-heavy
/// `command`/`prompt` proof (e.g. a `cargo` build/test) could drive a real
/// pane there and get OOM-killed mid-run (see `PSMUX_SCROLLBACK_OOM.local.md`
/// and the linked upstream repro for the full analysis).
///
/// `15000` is chosen to sit safely above every consumer that currently reads
/// pane scrollback depth, so lowering it costs no functional coverage:
/// - `runner.rs`'s poll loop and reattach capture read `capture_pane(...,
///   10_000)` — the deepest window anything reads. This is the hard floor:
///   do not lower `TMUX_HISTORY_LIMIT` below it without first lowering that
///   capture window to match, or the done-sentinel/event scan can miss
///   output that scrolled past between polls.
/// - the durable terminal log keeps `max_lines_per_attempt()` (default 4000)
///   and the pane snapshot keeps `PANE_SNAPSHOT_MAX_LINES` (4000) — both well
///   under this value.
/// - the board's live-view/history HTTP endpoint defaults to 2000 lines
///   (user-overridable).
///
/// At `-x 500`, this drops the per-pane ceiling from ~4.4 GB to ~330 MB — the
/// same order of magnitude already used successfully by every other
/// consumer in this list. This is a stopgap, not the permanent fix: per-pane
/// limits do not bound *aggregate* memory under concurrency (a pane's
/// ceiling times `DEFAULT_MAX_CONCURRENT` panes can still be large). The
/// permanent fix moves the durable transcript off pane scrollback entirely
/// (`pipe-pane` to a file — see `PSMUX_MEMORY_FIX.local.md` Phase 2), at
/// which point this constant can drop further, to just the live scrollback
/// window a human wants when opening Live View.
const TMUX_HISTORY_LIMIT: &str = "15000";

/// Overrides tmux resolution entirely — set to the full path (or bare name,
/// if it's on `PATH` under a different name) of the tmux-compatible binary to
/// use, skipping both the `PATH` lookup and the embedded fallback.
pub const TMUX_CMD_ENV: &str = "RALPHUS_TMUX_CMD";

/// Serializes every test that touches *real*, machine-wide tmux.exe process
/// state — both the live round-trip tests below and `server.rs`'s
/// `POST /api/daemon/shutdown` tests, which (like production) call
/// [`force_kill_tmux_processes`] and would otherwise race a concurrently
/// running `live_tmux_*` test in the same `cargo test` process, killing its
/// session out from under it. Production code never touches this — it's
/// only meaningful because `cargo test` runs tests from every module in one
/// shared process with real OS state.
#[cfg(test)]
pub(crate) static LIVE_TMUX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Unique per-invocation tag for a live-tmux test's session/run identifiers
/// (RAL-177): embeds the OS PID and a monotonic in-process counter, so two
/// `cargo test` processes running the identical test suite — e.g. sibling
/// git worktrees, each exercising these tests against the same shared,
/// machine-wide psmux server — can never construct the same real tmux
/// session name, and two tests within the same process never race each
/// other on a name either. Shared home for every live-tmux test across the
/// crate (`runner.rs`, `server.rs`, and this module's own tests), rather
/// than duplicated per file, since all of them already reach into
/// `crate::tmux::` for `LIVE_TMUX_TEST_LOCK`/`Tmux::resolve()`/
/// `session_name()` anyway. Same `{label}-{pid}-{n}` idiom already used by
/// `worktrees.rs`/`guardian_merge.rs`/`reviews.rs`/`scheduler.rs`'s test
/// helpers for the identical class of problem (temp-dir collisions across
/// concurrent test processes) — reused here instead of adding a `rand`
/// dependency the workspace doesn't otherwise have.
/// The PID is embedded as a `pid<N>` token (rather than a bare number) so
/// [`extract_test_pid`] can find and parse it back out of a full session
/// name unambiguously, for the orphan sweep in [`sweep_dead_test_sessions`].
#[cfg(test)]
pub(crate) fn unique_test_tag(label: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{label}-pid{}-{n}", std::process::id())
}

/// Best-effort extraction of the PID [`unique_test_tag`] embedded in a live-
/// tmux test's session name (RAL-177) — looks for the last `pid<digits>`
/// token in `name`, since a test session name may embed the tag in its
/// run_id segment, its task segment, or both. `None` for a name that was
/// never built from [`unique_test_tag`] at all (e.g. a real production
/// session, or a stray unrelated tmux session on the same machine) — the
/// sweep in [`sweep_dead_test_sessions`] only ever touches a name this
/// successfully parses, so it can't accidentally reach a session it can't
/// positively identify as one of its own test fixtures.
#[cfg(test)]
fn extract_test_pid(name: &str) -> Option<u32> {
    let mut best: Option<u32> = None;
    let mut rest = name;
    while let Some(idx) = rest.find("pid") {
        let digits: String = rest[idx + 3..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(pid) = digits.parse() {
            best = Some(pid);
        }
        rest = &rest[idx + 3..];
    }
    best
}

/// Whether OS process `pid` is still alive — best-effort, and fails *open*
/// (treats a lookup failure as "alive") so [`sweep_dead_test_sessions`] never
/// mistakenly kills a session whose owning process it simply couldn't check.
/// `pub(crate)` so `daemon/src/runner.rs`'s live-tmux tests can reuse it to
/// assert real OS process death after cancel (RAL-321), instead of
/// reimplementing the same platform-specific liveness check.
#[cfg(test)]
pub(crate) fn test_pid_is_alive(pid: u32) -> bool {
    if cfg!(target_os = "windows") {
        let script = format!(
            "(Get-Process -Id {pid} -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Id)"
        );
        match Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .stdin(Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => {
                !String::from_utf8_lossy(&out.stdout).trim().is_empty()
            }
            _ => true,
        }
    } else {
        // `kill -0` checks liveness without sending a real signal.
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// Force-kill every currently-registered `"ralphus_"`-prefixed live-tmux
/// TEST session (identified via [`extract_test_pid`]) whose owning test
/// process is confirmed dead (RAL-177 AC #3) — left behind by a `cargo test`
/// run that was killed, crashed, or hit a hard external timeout before its
/// own [`KillSessionOnDrop`] guard ever ran (Drop does not run on a hard
/// process kill). Unlike [`reap_orphaned_sessions_at_startup`] (which is
/// only ever safe to call before anything has been dispatched), this checks
/// each candidate session's own embedded PID for liveness directly instead
/// of relying on a "nothing should be running yet" invariant, so it's safe
/// to call opportunistically from *within* a live test run, not just at
/// startup — a session still backed by a live sibling test process is never
/// touched. Returns the number of sessions killed, for the caller to log.
#[cfg(test)]
pub(crate) fn sweep_dead_test_sessions() -> usize {
    let Ok(tmux) = Tmux::resolve() else {
        return 0;
    };
    let names = tmux
        .list_sessions_with_prefix("ralphus_")
        .unwrap_or_default();
    let mut killed = 0;
    for name in names {
        let Some(pid) = extract_test_pid(&name) else {
            continue;
        };
        if !test_pid_is_alive(pid) {
            let _ = tmux.kill_session(&name);
            killed += 1;
        }
    }
    killed
}

/// Runs [`sweep_dead_test_sessions`] exactly once per test binary invocation
/// (RAL-177) — cheap to call from every live-tmux test's own availability
/// check (`tmux_and_python_available()` in `runner.rs`, `tmux_available()`
/// in `server.rs`, and this module's own live tests) without repeating the
/// sweep's `list-sessions` + per-candidate liveness round trip on every
/// single test.
#[cfg(test)]
pub(crate) fn sweep_dead_test_sessions_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let killed = sweep_dead_test_sessions();
        if killed > 0 {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [tmux] swept {killed} orphaned live-tmux TEST session(s) left behind by a killed/timed-out prior test run (RAL-177)"
            );
        }
    });
}

/// Kills a real tmux session on drop (covers both normal test-fn return and
/// a panic/unwind mid-test) — the primary half of RAL-177 AC #3, alongside
/// the PID-liveness sweep above for the "process was hard-killed, Drop never
/// ran at all" case. Idempotent: killing an already-gone session is already
/// a no-op success (see [`Tmux::kill_session`]'s doc comment), so it's
/// always safe to hold one of these even when the test itself already killed
/// the session on its own successful path.
#[cfg(test)]
pub(crate) struct KillSessionOnDrop(pub(crate) String);

#[cfg(test)]
impl Drop for KillSessionOnDrop {
    fn drop(&mut self) {
        if let Ok(tmux) = Tmux::resolve() {
            let _ = tmux.kill_session(&self.0);
        }
    }
}

/// A tmux invocation failed, or the binary could not be resolved/spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxError(pub String);

impl std::fmt::Display for TmuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TmuxError {}

/// Deterministic, collision-avoiding tmux session name for a runner
/// invocation. `run_id`/`task`/`session_id` is the same triple every call
/// site already uses to key a [`crate::runner::RunnerSpec`] (scheduler
/// sessions, `prompt`-kind verify steps, and Guardian merge/resolver/
/// synthesizer sessions all set distinct values here), so no new bookkeeping
/// is needed to later recompute the same name for the HTTP capture-pane
/// endpoint (RAL-102 — the ticket leaves the naming scheme to implementation
/// judgment, "best-effort... no specific scheme mandated").
///
/// `task` is required, not optional: `session_id` alone (e.g. the
/// conventional `"work"`) is routinely reused across every task in a run, so
/// keying on `(run_id, session_id)` alone made sibling tasks' tmux sessions
/// collide — concurrent `new-session` calls for the same name race, and only
/// one survives while the rest fail with a tmux/psmux error (discovered
/// empirically: a 5-task run sharing session id `"work"` left 4 of 5 sessions
/// dead on arrival).
#[must_use]
pub fn session_name(run_id: &str, task: &str, session_id: &str) -> String {
    let raw = format!("ralphus_{run_id}_{task}_{session_id}");
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // tmux (and its Windows alternatives) get unwieldy with very long session
    // names; truncate defensively. Collisions are astronomically unlikely
    // since the run/session ids embedded here are already unique.
    const MAX_LEN: usize = 200;
    if sanitized.len() > MAX_LEN {
        sanitized[..MAX_LEN].to_string()
    } else {
        sanitized
    }
}

/// Directory pane snapshots (see [`write_pane_snapshot`]) live under —
/// `state_dir()` (next to the SQLite DB), not the OS temp dir the
/// spec/result side-channel files use, since a snapshot is meant to survive
/// well past the session ending (and a daemon restart), not just long enough
/// for one runner invocation to hand off its result.
///
/// Not yet cleaned up on run/guardian deletion — each snapshot is a small,
/// bounded text file (`PANE_SNAPSHOT_MAX_LINES`), so this is a modest,
/// bounded-per-entry disk-space cost over a long project lifetime, not the
/// unbounded resource leak documented for orphaned tmux/psmux processes in
/// `PSMUX_CRASH_NOTES.local.md`. Worth revisiting if it ever matters in
/// practice (e.g. tie deletion to `Store::delete_run`/`clear_all`/
/// `guardian_delete`, which would need to enumerate each deleted entity's
/// session names before their rows are gone).
fn pane_snapshot_dir() -> PathBuf {
    crate::state_dir().join("pane_snapshots")
}

/// Path a session's persisted last-pane-content snapshot lives (or would
/// live) at within `dir`, keyed by its already-sanitized deterministic tmux
/// session name (see [`session_name`]) — the same name every "peek"/"open
/// terminal" endpoint already recomputes to address a *live* session, reused
/// here so no new bookkeeping is needed to find the historical record once
/// the live one is gone. Takes `dir` explicitly (rather than always calling
/// [`pane_snapshot_dir`] internally) purely so [`write_pane_snapshot`]/
/// [`read_pane_snapshot`]'s tests can point it at a throwaway directory
/// instead of the real `state_dir()`.
#[must_use]
fn pane_snapshot_path_in(dir: &std::path::Path, session_name: &str) -> PathBuf {
    dir.join(format!("{session_name}.txt"))
}

/// Path a session's persisted last-pane-content snapshot lives (or would
/// live) at — see [`pane_snapshot_path_in`].
#[must_use]
pub fn pane_snapshot_path(session_name: &str) -> PathBuf {
    pane_snapshot_path_in(&pane_snapshot_dir(), session_name)
}

/// Bound on a persisted snapshot's size (RAL-102 follow-up) — generous
/// enough to be a genuinely useful "what was last there" record, small
/// enough that a long project history's worth of snapshots doesn't grow
/// disk usage unboundedly the way keeping every full pane transcript
/// forever would. Mirrors the existing `tail_lines` truncation already
/// applied to a "no result file" failure's pane-tail diagnostic
/// (`daemon/src/runner.rs::read_tmux_result`), just with a larger budget
/// since this is the record a human actually reads after the fact, not a
/// one-line error-message addendum.
const PANE_SNAPSHOT_MAX_LINES: usize = 4000;

/// Implementation behind [`write_pane_snapshot`], taking `dir` explicitly so
/// it's unit-testable against a throwaway directory instead of the real
/// `state_dir()`.
fn write_pane_snapshot_in(dir: &std::path::Path, session_name: &str, content: &str) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus [tmux] could not create pane snapshot dir {}: {e}",
            dir.display()
        );
        return;
    }
    // RAL-264: scrub resolved `from_env` secret values out of the pane content
    // before persisting it — the pane can legitimately show the agent's own
    // `$env:... = 'sk-or-v1-...'` assignment, and a stale snapshot is exactly
    // the kind of durable artifact the leak must not survive in.
    //
    // RAL-247: additionally scrub credential env-var values by pattern, so a
    // secret value that was never registered is still caught before the pane
    // text ever hits disk.
    let content = crate::redact::redact_all(content);
    let redacted = ralphus_core::redact::redact_secrets(&content);
    let truncated = crate::runner::tail_lines(&redacted, PANE_SNAPSHOT_MAX_LINES);
    if let Err(e) = std::fs::write(pane_snapshot_path_in(dir, session_name), truncated) {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus [tmux] could not write pane snapshot for {session_name}: {e}"
        );
    }
}

/// Persist `content` (the last successfully captured pane content — see
/// `crate::runner::SubprocessRunner::run_via_tmux_attempt`) as the durable,
/// read-only historical record for `session_name`, truncated to
/// [`PANE_SNAPSHOT_MAX_LINES`]. Called unconditionally at the end of every
/// tmux attempt (done, failed, cancelled, or timed out) so "what was last
/// there" is always available once the live session is gone — even for an
/// attempt that never printed anything, `content` may legitimately be empty,
/// which simply overwrites any stale prior snapshot with an empty one rather
/// than leaving it stuck showing an older attempt's output. Best-effort: a
/// write failure (e.g. disk full) is logged but never fails the session
/// itself, since the snapshot is a diagnostic convenience, not part of the
/// session's actual result.
pub fn write_pane_snapshot(session_name: &str, content: &str) {
    write_pane_snapshot_in(&pane_snapshot_dir(), session_name, content);
}

/// Implementation behind [`read_pane_snapshot`], taking `dir` explicitly so
/// it's unit-testable against a throwaway directory instead of the real
/// `state_dir()`.
fn read_pane_snapshot_in(dir: &std::path::Path, session_name: &str) -> Option<String> {
    // RAL-247: redact on read too — a snapshot written before this fix (or by
    // a version without it) may already carry a secret value on disk.
    std::fs::read_to_string(pane_snapshot_path_in(dir, session_name))
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| ralphus_core::redact::redact_secrets(&s).into_owned())
}

/// Read back a session's persisted last-pane-content snapshot, if one was
/// ever written (see [`write_pane_snapshot`]). `None` when no snapshot
/// exists (the session never ran under tmux, or its attempt(s) produced no
/// pane output at all) — the caller degrades to its own "nothing to show"
/// behavior in that case, never treating a missing file as an error.
#[must_use]
pub fn read_pane_snapshot(session_name: &str) -> Option<String> {
    read_pane_snapshot_in(&pane_snapshot_dir(), session_name)
}

/// Strip genuinely empty trailing rows from a raw `capture-pane` result —
/// see [`Tmux::capture_pane`]'s doc comment for why they show up at all.
/// Trims trailing newlines/carriage-returns/spaces/tabs off the end of the
/// whole string (equivalent to dropping trailing blank lines one at a time),
/// leaving interior blank lines — genuine output the agent printed — and the
/// last real line's own content untouched.
fn trim_trailing_blank_pane_lines(raw: &str) -> String {
    raw.trim_end_matches(['\n', '\r', ' ', '\t']).to_string()
}

/// Quote `arg` for the shell that will type/receive it: PowerShell
/// single-quote escaping on Windows (session start commands are delivered via
/// `send-keys` into a PowerShell pane there — see [`Tmux::new_detached_session_with_command`]),
/// POSIX single-quote escaping elsewhere (delivered via `respawn-pane` into
/// `$SHELL -c`).
fn quote_for_shell(arg: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("'{}'", arg.replace('\'', "''"))
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Build a single quoted command-line string from a program and its
/// arguments, suitable for [`Tmux::new_detached_session_with_command`].
///
/// On Windows this is typed into an interactive PowerShell pane via
/// `send-keys` (see [`Tmux::new_detached_session_with_command`]'s doc
/// comment) — and PowerShell does not invoke a quoted string as a command
/// name on its own (`'python' '-c' '...'` is a parse error: `'python'` is
/// just a string expression, not a command invocation), so the line is
/// prefixed with the call operator `&`, exactly as gastown's own Windows
/// port does for the same reason (`config/loader.go`: `"& '<scriptPath>'"`).
/// Verified empirically against the Windows tmux build this project targets
/// while implementing RAL-102.
#[must_use]
pub fn build_command_line(program: &str, args: &[String]) -> String {
    let parts = std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(quote_for_shell)
        .collect::<Vec<_>>()
        .join(" ");
    if cfg!(target_os = "windows") {
        format!("& {parts}")
    } else {
        parts
    }
}

/// Like [`build_command_line`], additionally prefixing `env`'s assignments so
/// they're set in the pane's shell before the command runs (RAL-150). The
/// tmux-wrapped runner path has no `std::process::Command::envs`-style hook —
/// the pane executes a typed/`respawn-pane`-fed command line via its own
/// shell, not a `Command` this process builds directly — so overrides are
/// embedded in that same line instead. `env` iterates in `BTreeMap` order for
/// a deterministic line.
///
/// Every key **must** already be a validated identifier
/// ([`crate::config::is_valid_env_key`]) — enforced at the HTTP boundary
/// (`crate::server`'s env-override handler) before an override ever reaches
/// the store. Keys are interpolated unquoted (`$env:KEY = ...` /
/// `KEY=... cmd`, neither of which accepts a quoted variable name), so this
/// is the one place downstream of that boundary a bad key could still turn
/// into shell injection; entries with an invalid key are dropped rather than
/// trusted, as a defense-in-depth backstop should that invariant ever slip.
/// Values are always quoted via [`quote_for_shell`], same as every other
/// argument on the line.
#[must_use]
pub fn build_command_line_with_env(
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> String {
    let base = build_command_line(program, args);
    let valid: Vec<(&String, &String)> = env
        .iter()
        .filter(|(k, _)| crate::config::is_valid_env_key(k))
        .collect();
    if valid.is_empty() {
        return base;
    }
    if cfg!(target_os = "windows") {
        let assigns: String = valid
            .iter()
            .map(|(k, v)| format!("$env:{k} = {}; ", quote_for_shell(v)))
            .collect();
        // `base` already starts with the PowerShell call operator `&`.
        format!("{assigns}{base}")
    } else {
        let assigns: String = valid
            .iter()
            .map(|(k, v)| format!("{k}={} ", quote_for_shell(v)))
            .collect();
        format!("{assigns}{base}")
    }
}

/// Rendered `-e KEY=value` flag pairs for `new-session` (RAL-247), one per
/// validated override in `env`. Values are passed to tmux as separate
/// arguments, so unlike the inline-command form they are never typed into the
/// pane and never echoed. Keys use the same
/// [`crate::config::is_valid_env_key`] filter as [`build_command_line_with_env`],
/// and values were already control-character-checked at the HTTP boundary
/// (RAL-227), so this is a pure mapping over already-sanitized entries.
fn env_override_flags(env: &BTreeMap<String, String>) -> Vec<String> {
    let mut flags = Vec::with_capacity(env.len() * 2);
    for (k, v) in env
        .iter()
        .filter(|(k, _)| crate::config::is_valid_env_key(k))
    {
        flags.push("-e".to_string());
        flags.push(format!("{k}={v}"));
    }
    flags
}

/// Search `PATH` by hand (no extra dependency) for an executable named
/// `program`, trying common Windows executable suffixes there.
fn find_on_path(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let suffixes: &[&str] = if cfg!(target_os = "windows") {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path_var) {
        for suffix in suffixes {
            let candidate = dir.join(format!("{program}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve which tmux-compatible binary to invoke, per the priority in the
/// module doc comment.
///
/// # Errors
/// Returns an error when no override is set, nothing named `tmux` is on
/// `PATH`, and no embedded fallback is available in this build.
pub fn resolve_tmux_program() -> Result<String, TmuxError> {
    if let Ok(cmd) = std::env::var(TMUX_CMD_ENV) {
        return Ok(cmd);
    }
    if find_on_path("tmux").is_some() {
        return Ok("tmux".to_string());
    }
    embedded::extract().map(|p| p.to_string_lossy().into_owned())
}

/// Session-name-keyed registry of the Windows Job Objects
/// [`Tmux::kill_session`] uses to actually terminate a session's real
/// process tree (RAL-321) — see the module doc comment's "Process-tree
/// confinement on cancel" section. Keyed by session name rather than held on
/// `Tmux` itself since [`Tmux::resolve`]/[`Tmux::from_program`] are cheap and
/// called fresh at every call site (no long-lived instance to hang state
/// off); a session name is already the daemon-wide-unique key
/// [`Tmux::kill_session`] has to work with.
#[cfg(windows)]
mod confine {
    use std::collections::HashMap;
    use std::sync::Mutex;

    static JOBS: Mutex<Option<HashMap<String, win32job::Job>>> = Mutex::new(None);

    fn lock() -> std::sync::MutexGuard<'static, Option<HashMap<String, win32job::Job>>> {
        JOBS.lock().expect("tmux job registry poisoned")
    }

    /// Confine `child` (the `new-session` client) to a fresh kill-on-close
    /// job and remember it under `name`. Must be called as soon as possible
    /// after `spawn()` — before the client has had a chance to start the
    /// psmux server itself — so the server (and, transitively, everything it
    /// later spawns into the pane) inherits job membership at its own
    /// creation. Best-effort: a failure to create/assign the job is logged
    /// and simply leaves `name` unconfined; `kill_session` still frees the
    /// session name in that case, it just falls back to
    /// `force_kill_tmux_processes` alone, exactly as it did before this fix.
    pub(super) fn confine(name: &str, child: &std::process::Child) {
        use std::os::windows::io::AsRawHandle;
        let job = (|| -> Result<win32job::Job, win32job::JobError> {
            let job = win32job::Job::create()?;
            let mut info = win32job::ExtendedLimitInfo::new();
            info.limit_kill_on_job_close();
            job.set_extended_limit_info(&info)?;
            job.assign_process(child.as_raw_handle() as isize)?;
            Ok(job)
        })();
        match job {
            Ok(job) => {
                lock()
                    .get_or_insert_with(HashMap::new)
                    .insert(name.to_string(), job);
            }
            Err(e) => {
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                crate::rlog!(
                    WARNING,
                    "ralphus [tmux] could not confine session {name} to a job object, cancellation may not reach its full process tree: {e}"
                );
            }
        }
    }

    /// Drop `name`'s registered job, if any — `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
    /// means dropping the job's only handle terminates every process ever
    /// assigned to (or spawned under, via inheritance into) it, including
    /// the psmux server itself. A no-op if `name` was never confined (job
    /// creation failed at spawn time) or was already reaped by an earlier
    /// call — [`Tmux::kill_session`] can be invoked more than once per
    /// attempt.
    pub(super) fn kill(name: &str) {
        if let Some(map) = lock().as_mut() {
            map.remove(name);
        }
    }
}

#[cfg(not(windows))]
mod confine {
    pub(super) fn confine(_name: &str, _child: &std::process::Child) {}
    pub(super) fn kill(_name: &str) {}
}

/// Splits a command-line string like `"tmux"` or `"wsl.exe tmux"` into a
/// program and any leading fixed arguments — the same whitespace-split
/// convention `RALPHUS_RUNNER_CMD`/`SubprocessRunner::new` already uses for
/// the runner command, applied here so `RALPHUS_TMUX_CMD` (and the resolved
/// binary generally) can name a program *plus* arguments — e.g. routing
/// through `wsl.exe tmux` to test real upstream tmux — instead of only a
/// bare executable. A plain path with no spaces splits into itself with an
/// empty argument list, so this is a no-op for the common case.
fn split_command(command_line: &str) -> (String, Vec<String>) {
    let mut parts = command_line.split_whitespace().map(str::to_string);
    let program = parts.next().unwrap_or_default();
    (program, parts.collect())
}

/// A resolved tmux binary, ready to run commands against.
pub struct Tmux {
    program: String,
    /// Fixed arguments that must precede every subcommand (e.g. `["tmux"]`
    /// when routed through `wsl.exe tmux`) — see [`split_command`].
    prefix_args: Vec<String>,
}

impl Tmux {
    /// Resolve the tmux binary per [`resolve_tmux_program`].
    ///
    /// # Errors
    /// See [`resolve_tmux_program`].
    pub fn resolve() -> Result<Self, TmuxError> {
        Ok(Self::from_program(resolve_tmux_program()?))
    }

    /// Build directly from an already-resolved command line (mainly for
    /// tests and the `ralphus-daemon mux` passthrough, which resolves once
    /// up front). May be a bare program (`"tmux"`) or a program plus leading
    /// arguments (`"wsl.exe tmux"`) — see [`split_command`].
    #[must_use]
    pub fn from_program(command_line: impl Into<String>) -> Self {
        let (program, prefix_args) = split_command(&command_line.into());
        Self {
            program,
            prefix_args,
        }
    }

    /// The resolved program path/name this instance invokes — for callers
    /// (like the "open terminal" HTTP handler) that need to spawn tmux
    /// themselves (e.g. `tmux attach-session`) rather than call through this
    /// wrapper. Pair with [`Self::prefix_args`] — a multi-word
    /// `RALPHUS_TMUX_CMD` needs both, not just the program name, or its
    /// leading arguments are silently lost for such a caller.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Fixed leading arguments that must precede every subcommand (empty for
    /// the common bare-executable case). See [`Self::program`]'s doc comment
    /// for why an external caller building its own `Command` needs these
    /// too.
    #[must_use]
    pub fn prefix_args(&self) -> &[String] {
        &self.prefix_args
    }

    fn run(&self, args: &[&str]) -> Result<String, TmuxError> {
        let output = Command::new(&self.program)
            .args(&self.prefix_args)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| TmuxError(format!("could not run '{}': {e}", self.program)))?;
        if !output.status.success() {
            return Err(TmuxError(format!(
                "tmux {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Like [`Self::run`], but confines the spawned client process to a
    /// fresh Windows Job Object under `session_name` before waiting on it —
    /// see the module doc comment's "Process-tree confinement on cancel"
    /// section. Used only for the `new-session` call in
    /// [`Self::new_detached_session_with_command`], the one client spawn
    /// whose process tree must survive to be killable later. A no-op on
    /// non-Windows (confinement doesn't exist there — see [`confine`]).
    fn run_confined(&self, args: &[&str], session_name: &str) -> Result<String, TmuxError> {
        let child = Command::new(&self.program)
            .args(&self.prefix_args)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| TmuxError(format!("could not run '{}': {e}", self.program)))?;
        confine::confine(session_name, &child);
        let output = child
            .wait_with_output()
            .map_err(|e| TmuxError(format!("could not run '{}': {e}", self.program)))?;
        if !output.status.success() {
            return Err(TmuxError(format!(
                "tmux {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Whether a session with this exact name currently exists.
    #[must_use]
    pub fn has_session(&self, name: &str) -> bool {
        self.run(&["has-session", "-t", name]).is_ok()
    }

    /// Names of all currently-registered sessions starting with `prefix`.
    /// Used to find every tmux session belonging to a run (or a narrower
    /// scope within it) without needing to precisely reconstruct each
    /// individual session/verify name from first principles — see
    /// [`Self::kill_sessions_with_prefix`].
    ///
    /// # Errors
    /// Returns an error if `list-sessions` itself fails (e.g. no server
    /// running at all) — treated the same as "no sessions" by callers that
    /// only care about best-effort cleanup.
    pub fn list_sessions_with_prefix(&self, prefix: &str) -> Result<Vec<String>, TmuxError> {
        let output = self.run(&["list-sessions"])?;
        Ok(output
            .lines()
            .filter_map(|line| line.split(':').next())
            .filter(|name| name.starts_with(prefix))
            .map(str::to_string)
            .collect())
    }

    /// Kill every currently-registered session whose name starts with
    /// `prefix`. Best-effort: a failure killing one name doesn't stop the
    /// rest, and a `list-sessions` failure (e.g. nothing running at all) is
    /// treated as "nothing to kill", not an error. Returns how many
    /// sessions matched (attempted), for callers that log a count.
    pub fn kill_sessions_with_prefix(&self, prefix: &str) -> usize {
        let names = self.list_sessions_with_prefix(prefix).unwrap_or_default();
        let count = names.len();
        for name in names {
            let _ = self.kill_session(&name);
        }
        count
    }

    /// Create a detached session named `name` rooted at `cwd`, with
    /// `remain-on-exit` enabled, then start the program `program` with `args`
    /// as the pane's process.
    ///
    /// Real tmux supports replacing the initial shell in one step
    /// (`respawn-pane <command>`); the Windows-alternative tmux build this
    /// project targets does not execute a command argument passed to
    /// `respawn-pane` (verified empirically — the call succeeds but the shell
    /// prompt is left running), so on Windows the command is instead typed
    /// into the shell via `send-keys` — the same fallback gastown's own
    /// Windows port uses for the identical gap.
    ///
    /// `env` holds RAL-150-style per-session environment overrides. Since a
    /// typed `send-keys` line is echoed verbatim into the pane, inlining them
    /// as `$env:KEY = 'value'` assignments would leak their values into pane
    /// captures (RAL-247). On Windows they are therefore delivered through
    /// `new-session -e KEY=value` instead, which seeds the pane shell's own
    /// environment — the value never becomes pane text, yet the launched
    /// program (and anything it spawns) still inherits it. On POSIX the
    /// `respawn-pane` path never echoes, so overrides are kept inlined in the
    /// command (unchanged, and guaranteed to reach the process); `-e` is not
    /// used there so no POSIX-only tmux behavior is relied on.
    ///
    /// Because the Windows launch line must stay free of secrets, the caller
    /// passes `program`/`args` rather than a pre-rendered command string: the
    /// method renders the bare command for `send-keys` (Windows) and the
    /// env-inlined command for `respawn-pane` (POSIX) itself, so the two can
    /// never drift.
    ///
    /// # Errors
    /// Returns an error if any of the underlying tmux calls fail; the
    /// partially-created session is killed before returning so a failed
    /// start never leaks a zombie session.
    pub fn new_detached_session_with_command(
        &self,
        name: &str,
        cwd: &str,
        env: &BTreeMap<String, String>,
        program: &str,
        args: &[String],
    ) -> Result<(), TmuxError> {
        let base = build_command_line(program, args);
        let full = build_command_line_with_env(program, args, env);
        // Deliberately very wide (default is much narrower) so a long line --
        // e.g. a Bash tool call's rendered command/description in the live
        // pane -- doesn't get hard-wrapped by the pane itself on top of the
        // runner's own 80-char truncation, which made captured output nearly
        // unreadable (two independent truncations stacking). No real
        // downside to going wide here: this pane is consumed via
        // `capture-pane` (an automated poll), not sat in front of by a human
        // at a fixed terminal width, so there's no reason to economize.
        let mut new_session_args: Vec<String> = vec![
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            name.to_string(),
            "-c".to_string(),
            cwd.to_string(),
            "-x".to_string(),
            "500".to_string(),
            "-y".to_string(),
            "50".to_string(),
        ];
        if cfg!(target_os = "windows") {
            new_session_args.extend(env_override_flags(env));
        }
        let ns_refs: Vec<&str> = new_session_args.iter().map(String::as_str).collect();
        // RAL-321: confine the `new-session` client at spawn time so
        // `kill_session` can later kill this session's whole real process
        // tree, not just free its name — see the module doc comment.
        self.run_confined(&ns_refs, name)?;
        // Best-effort: without this, tmux discards a dead pane's content
        // immediately, which would race the daemon's own sentinel-based
        // completion detection.
        let _ = self.run(&["set-option", "-t", name, "remain-on-exit", "on"]);
        // Best-effort: raise the scrollback limit past tmux's default (2000
        // lines) so `ralphus history --live` (RAL-140), which polls
        // `capture-pane` with a larger `lines` window than the board's
        // live-view default, doesn't silently lose older output to tmux's
        // own buffer trimming while a long-running session is still live.
        // See `TMUX_HISTORY_LIMIT`'s doc comment for why this value is
        // capped well below tmux's own maximum.
        let _ = self.run(&[
            "set-option",
            "-t",
            name,
            "history-limit",
            TMUX_HISTORY_LIMIT,
        ]);
        let started = if cfg!(target_os = "windows") {
            self.run(&["send-keys", "-t", name, base.as_str(), "Enter"])
        } else {
            self.run(&["respawn-pane", "-k", "-t", name, "-c", cwd, full.as_str()])
        };
        if let Err(e) = started {
            let _ = self.kill_session(name);
            return Err(e);
        }
        Ok(())
    }

    /// Type literal text into `name`'s pane, followed by a separate `Enter`
    /// keypress — more reliable than appending Enter to the same `send-keys`
    /// call (mirrors gastown's `SendKeysDebounced`). Not used by the
    /// tmux-wrapped runner path (which delivers its command at session
    /// creation, see [`Self::new_detached_session_with_command`]); kept for
    /// the interactive "open terminal" attach path and for `ralphus-daemon
    /// mux` passthrough use.
    ///
    /// # Errors
    /// Returns an error if either underlying `send-keys` call fails.
    pub fn send_keys_literal(&self, name: &str, text: &str) -> Result<(), TmuxError> {
        self.run(&["send-keys", "-t", name, "-l", text])?;
        std::thread::sleep(Duration::from_millis(100));
        self.run(&["send-keys", "-t", name, "Enter"])?;
        Ok(())
    }

    /// Capture the last `lines` lines of `name`'s pane content, without
    /// attaching (mirrors gastown's `gt peek` / `CapturePane`).
    ///
    /// `capture-pane -S <start>` with no `-E` (end) argument captures up to
    /// the bottom of the pane's current on-screen view, not the last line of
    /// real output — for a session whose output is shorter than the pane's
    /// height (`-y 50`, see [`Self::new_detached_session_with_command`]),
    /// that means genuinely empty trailing rows come back as part of the
    /// content (confirmed against this project's psmux build: a 3-line pane
    /// capture inside a 24-row pane returned 21 trailing blank lines).
    /// [`trim_trailing_blank_pane_lines`] strips that padding so every
    /// consumer (the live-view JSON, terminal-log persistence, the
    /// done-sentinel poll) sees exactly the real transcript, RAL-237.
    ///
    /// # Errors
    /// Returns an error if the session does not exist or tmux fails.
    pub fn capture_pane(&self, name: &str, lines: u32) -> Result<String, TmuxError> {
        self.run(&["capture-pane", "-p", "-t", name, "-S", &format!("-{lines}")])
            .map(|raw| trim_trailing_blank_pane_lines(&raw))
    }

    /// Kill `name` if it exists. Idempotent: a session that's already gone is
    /// treated as success, since every caller uses this for best-effort
    /// cleanup.
    ///
    /// Blocks until `has-session` confirms the name is actually free (bounded
    /// by `KILL_CONFIRM_TIMEOUT`) rather than returning as soon as
    /// `kill-session` exits. On the Windows tmux-alternative build this
    /// project targets (psmux), `kill-session` tears the server down
    /// asynchronously: it exits 0 while the session's name is still
    /// registered, so an immediate `new-session` reusing that name is racy —
    /// it can either fail outright (`duplicate session`) or appear to
    /// succeed and then have its `send-keys` fail moments later (`no server
    /// running on session`) because the just-killed server raced the new
    /// one for the same name. Empirically: 5/15 immediate re-creates failed
    /// without this wait, 0/15 failed with it (see `daemon/src/runner.rs`'s
    /// stale-session-before-resume call site, the only caller that
    /// immediately reuses the name it just killed).
    pub fn kill_session(&self, name: &str) -> Result<(), TmuxError> {
        if !self.has_session(name) {
            // Still drop any job registered for `name` (RAL-321) even though
            // tmux itself already forgot the session — otherwise a caller
            // that calls this on an already-gone name (a legitimate,
            // idempotent use per this method's own doc comment) leaks that
            // map entry for the rest of the daemon's lifetime.
            confine::kill(name);
            return Ok(());
        }
        self.run(&["kill-session", "-t", name])?;
        const KILL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
        const POLL_INTERVAL: Duration = Duration::from_millis(50);
        let started = Instant::now();
        while self.has_session(name) {
            if started.elapsed() >= KILL_CONFIRM_TIMEOUT {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        // RAL-321: drop this session's confining job (if
        // `new_detached_session_with_command` managed to create one at spawn
        // time) *before* the belt-and-suspenders `force_kill_tmux_processes`
        // sweep below. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` terminates the
        // whole tree the job ever gained a member in -- the psmux server and
        // everything it later spawned into the pane (the persistent shell,
        // the injected `ralphus-runner.exe`, the agent CLI or `cmd /C
        // <command>`, and their own children) -- which
        // `force_kill_tmux_processes`'s literal `tmux.exe`-name match alone
        // can never reach. See the module doc comment.
        confine::kill(name);
        // `kill-session` on the Windows tmux-alternative this project targets
        // (psmux) frees the *name* from its own registry but never actually
        // terminates the backing OS process (confirmed: see
        // "BREAKTHROUGH: kill-session leaks the underlying OS process" in
        // PSMUX_CRASH_NOTES.local.md -- sessions explicitly killed hours
        // earlier were still alive as real `tmux.exe` processes, spinning
        // CPU). `has_session(name)` is confirmed false above, so any
        // `tmux.exe` still alive under this exact deterministic session name
        // is unambiguously a zombie, never a live session we might still
        // need -- safe to force-terminate directly. Kept as a fallback for
        // sessions the job above either wasn't attached to (confinement
        // failed at spawn) or predate this fix -- it remains necessary, not
        // superseded by `confine::kill`.
        let killed = force_kill_tmux_processes(name);
        if killed > 0 {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [tmux] force-killed {killed} zombie tmux.exe process(es) left behind by kill-session for {name}"
            );
        }
        Ok(())
    }
}

/// Force-terminate every `tmux.exe` process whose command line contains
/// `needle` and looks like a server (`Where-Object`'s `-like '*server*'`
/// filter) -- the only way to actually free a process psmux's own
/// `kill-session` merely unregisters without killing (see
/// [`Tmux::kill_session`]'s doc comment and
/// `PSMUX_CRASH_NOTES.local.md`). No-op (returns `0`) on non-Windows
/// platforms and on any failure to run the lookup -- best-effort cleanup,
/// never load-bearing for a session's own result.
///
/// # Safety of the wildcard match
/// This has no way to distinguish a legitimate process from a zombie on its
/// own -- callers must only pass a `needle` that already makes every match
/// safe to kill: either an exact, just-confirmed-unregistered
/// [`session_name`] (as [`Tmux::kill_session`] does), or the shared
/// `"ralphus_"` prefix at daemon startup, before anything has been
/// dispatched (as [`reap_orphaned_sessions_at_startup`] does) -- never
/// psmux's own internal `__warm__` pool, whose claim protocol this project
/// doesn't own or fully understand.
///
/// RAL-234: this used to shell out to `powershell -Command "Get-CimInstance
/// Win32_Process ..."` per call -- exactly the pattern
/// [`find_server_pid_windows`] used to follow too, and was already replaced
/// there (see that function's doc comment for the measured before/after
/// numbers: PowerShell's own .NET startup cost, worse still under concurrent
/// subprocess load). This call in particular sits on the daemon-startup
/// critical path (`reap_orphaned_sessions_at_startup` runs synchronously
/// after the port is bound but before the daemon starts accepting/processing
/// requests), so its cost was confirmed as the dominant contributor to
/// "first page load after daemon start is slow" -- startup timing
/// instrumentation (`server.rs::serve`'s `startup` note) attributes the bulk
/// of the pre-serving delay to `tmux_reap_ms`. Reads the process table
/// directly via `sysinfo` (no subprocess) instead, the same fix for the same
/// reason.
fn force_kill_tmux_processes(needle: &str) -> usize {
    #[cfg(target_os = "windows")]
    {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        // `cmd()` is empty by default under `sysinfo` -- fetching a process's
        // command line is comparatively expensive, so it's opt-in via this
        // refresh-kind flag, same as `find_server_pid_windows`.
        let refresh_kind = ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always);
        let mut sys = System::new();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind);
        let mut killed = 0;
        for proc in sys.processes().values() {
            if !proc.name().eq_ignore_ascii_case("tmux.exe") {
                continue;
            }
            let cmd = proc
                .cmd()
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            if cmd.contains(needle) && cmd.contains("server") && proc.kill() {
                killed += 1;
            }
        }
        killed
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = needle;
        0
    }
}

/// Force-terminate every `ralphus_`-named `tmux.exe` process still alive,
/// unconditionally. Only ever safe to call at daemon startup, before the
/// scheduler has dispatched anything -- relies on the exact same "nothing is
/// executing yet, so anything found is orphaned" invariant
/// `Store::recover_orphaned_runs` already uses for `Running` DB rows,
/// applied here to OS processes instead: any `ralphus_`-prefixed tmux
/// session alive at this precise moment cannot belong to a session this
/// daemon process started, so it must be left over from a previous daemon
/// lifetime (crash, restart, or a `kill-session` that leaked per
/// [`force_kill_tmux_processes`]'s doc comment). Deliberately never touches
/// psmux's own separate `__warm__` pool processes -- a distinct, not fully
/// understood leak (see `PSMUX_CRASH_NOTES.local.md`) this project doesn't
/// own or control the claim protocol for.
///
/// Returns the number of processes killed, for the caller to log.
///
/// **Startup-only — do not call this from a live request handler.** It was
/// briefly (mis)used from `/api/daemon/shutdown`, which runs while real
/// `ralphus_<run_id>_...`-named sessions (including, when `cargo test`
/// itself runs inside a daemon-spawned pane, the very pane hosting the
/// call) are alive — the unscoped `*ralphus_*` match killed its own host.
/// See `PSMUX_CRASH_NOTES.local.md`'s "SOLVED" section and `server.rs`'s
/// `shutdown` doc comment. Use `server.rs`'s per-run/per-guardian scoped
/// kill helpers (`kill_run_tmux_sessions` / `kill_guardian_tmux_sessions`)
/// from any live-request context instead.
#[must_use]
pub fn reap_orphaned_sessions_at_startup() -> usize {
    force_kill_tmux_processes("ralphus_")
}

/// How a tracked tmux-server process actually ended, for sessions that die
/// with no explanation (see `PSMUX_CRASH_NOTES.local.md`). Windows Event
/// Viewer never shows anything for these deaths (checked repeatedly against
/// known failure windows -- zero entries), and the natural way to get a real
/// exit code (`Win32_ProcessStartTrace`/`StopTrace`) needs Administrator
/// privileges and is limited to one consumer system-wide (confirmed blocked
/// in practice). [`watch_for_exit`] gets the same information a different,
/// unprivileged way: by holding an actual process handle via .NET
/// (`System.Diagnostics.Process.WaitForExit`/`.ExitCode`) in a short-lived
/// helper process, which needs no elevation and isn't subject to that
/// single-consumer limitation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessExit {
    /// The process exited with this code. `0` is a normal/clean exit; a
    /// large value (e.g. `3221225477` / `0xC0000005`) is a Windows exception
    /// code -- an access violation in this example -- i.e. a real crash, not
    /// a program-chosen exit.
    Exited(i64),
    /// The exit code couldn't be determined. Carries a short, best-effort
    /// reason string -- e.g. the raw exception message from a failed
    /// `Get-Process`/`WaitForExit()` call (distinguishing "no such process"
    /// from "access denied" from a parse failure), or a fixed description
    /// when the `powershell` helper itself couldn't be spawned at all.
    /// Absence of a value is never an error -- this is a best-effort
    /// diagnostic, never load-bearing for the session's own result.
    Unknown(String),
}

/// Best-effort lookup of the OS process id backing session `name`'s live
/// tmux server, by matching a `tmux.exe server -s <name> ...` command line.
/// Returns `None` on any failure or on non-Windows platforms (this whole
/// mechanism only exists to work around a Windows-specific diagnostic gap).
///
/// Uses `Get-CimInstance` (plain WMI, no admin rights needed) rather than
/// `Win32_ProcessStartTrace`/`StopTrace` (event *subscription*, which does
/// need admin and is limited to one consumer system-wide) -- this is a
/// point-in-time query, not a live subscription, so neither limitation
/// applies.
#[must_use]
pub fn find_server_pid(name: &str) -> Option<u32> {
    if !cfg!(target_os = "windows") {
        return None;
    }
    let pid = find_server_pid_windows(name);
    // Logged unconditionally (not just on the eventual failure path) so a
    // real occurrence always leaves a record of which PID was tracked --
    // useful for cross-referencing manually (e.g. a live process-watch
    // script) even when `watch_for_exit`'s own diagnostic later comes back
    // `Unknown`.
    match pid {
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        Some(p) => crate::rlog!(
            DEBUG,
            "ralphus [runner] found tmux server pid={p} for session={name}"
        ),
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        None => crate::rlog!(
            DEBUG,
            "ralphus [runner] could not find tmux server pid for session={name} (no matching process)"
        ),
    }
    pid
}

/// The actual Windows process-table scan behind [`find_server_pid`], split
/// out so it can be compiled away entirely on non-Windows (where `sysinfo`
/// isn't even a dependency -- see `[target."cfg(windows)".dependencies]` in
/// `Cargo.toml`).
///
/// This used to shell out to `powershell -Command "Get-CimInstance
/// Win32_Process ..."` per lookup. That was measurably the single slowest
/// piece of this whole subsystem: PowerShell's own .NET startup cost is
/// substantial even alone, and degrades sharply under the heavy concurrent
/// subprocess-spawning a full `cargo test` run produces -- two individual
/// tests (`live_tmux_registers_pid_while_the_subprocess_is_alive_and_clears_it_after`,
/// `live_tmux_registers_and_clears_pid_for_a_command_kind_spec`) were
/// measured at 31s and 28s respectively, dominated by this call. `sysinfo`
/// reads the Windows process table directly (via `ntapi`/`winapi`, no
/// subprocess), which is the same fix for production latency as it is for
/// test time -- `find_server_pid` sits on the hot path of every tmux-wrapped
/// session spawn (RAL-151), not just these tests.
///
/// `name` is always `tmux::session_name`'s output -- no injection risk here
/// since there's no shell/query language to escape into, just a plain
/// substring check against each process's own command line.
///
/// Retries a handful of times with a short sleep between attempts: the
/// caller invokes this immediately after the `tmux new-session` *client*
/// subprocess returns, but that only guarantees the client asked the tmux
/// *server* to start -- on a first session (no server running yet) there can
/// be a brief window where the server process hasn't finished forking/
/// exec'ing and so doesn't yet show up in a process-table snapshot. The old
/// PowerShell-based lookup's own multi-hundred-ms .NET startup cost
/// accidentally absorbed that race; a direct process-table read is fast
/// enough that it needs an explicit bounded retry instead. Capped well
/// below the old single-call cost (`RETRIES * RETRY_DELAY` = 500ms vs. the
/// ~1-2s+ a single PowerShell invocation used to take), so this is still a
/// large net win even in the worst case.
#[cfg(target_os = "windows")]
fn find_server_pid_windows(name: &str) -> Option<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    const RETRIES: u32 = 5;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
    // `cmd()` is empty by default under `sysinfo` -- fetching a process's
    // command line is comparatively expensive, so it's opt-in via this
    // refresh-kind flag. Without it every process silently reports `cmd:
    // []`, and the substring match below would never find anything.
    let refresh_kind = ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always);

    let mut sys = System::new();
    for attempt in 0..RETRIES {
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind);
        let found = sys.processes().values().find_map(|proc| {
            if !proc.name().eq_ignore_ascii_case("tmux.exe") {
                return None;
            }
            let cmd = proc
                .cmd()
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            (cmd.contains(name) && cmd.contains("server")).then(|| proc.pid().as_u32())
        });
        if found.is_some() {
            return found;
        }
        if attempt + 1 < RETRIES {
            std::thread::sleep(RETRY_DELAY);
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn find_server_pid_windows(_name: &str) -> Option<u32> {
    None
}

/// Spawns a background thread that blocks until OS process `pid` exits, then
/// sends the real [`ProcessExit`] it observed. The blocking wait happens
/// inside a short-lived `powershell` helper process (via a real `.NET`
/// process handle), not in this thread itself, so this call returns
/// immediately; read the result from the returned channel once the caller
/// knows the tracked session has ended (a bounded `recv_timeout` is
/// appropriate -- if the tracked process is confirmed gone, the helper
/// should already be finishing up or have finished).
#[must_use]
pub fn watch_for_exit(pid: u32) -> std::sync::mpsc::Receiver<ProcessExit> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The catch branch deliberately reports the real exception message
        // (`GONE:<message>`) rather than a fixed sentinel -- "no such
        // process" (the watcher lost the race, or the process truly never
        // existed) and "access is denied" (the process exists but this
        // helper can't open a handle to it, e.g. a different token/session)
        // produce identical *symptoms* today (session died mid-run) but
        // very different diagnoses, and collapsing them into one opaque
        // "Unknown" was itself the reason this instrumentation returned no
        // useful signal across its first three real occurrences -- see
        // PSMUX_CRASH_NOTES.local.md.
        let script = format!(
            "try {{ $p = Get-Process -Id {pid} -ErrorAction Stop; $p.WaitForExit(); Write-Output $p.ExitCode }} \
             catch {{ Write-Output \"GONE:$($_.Exception.Message)\" }}"
        );
        let result = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .stdin(Stdio::null())
            .output();
        let observation = match result {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                text.parse::<i64>().map_or_else(
                    |_| {
                        let reason = text
                            .strip_prefix("GONE:")
                            .map(str::to_string)
                            .unwrap_or(text);
                        ProcessExit::Unknown(reason)
                    },
                    ProcessExit::Exited,
                )
            }
            Ok(out) => ProcessExit::Unknown(format!(
                "powershell exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => ProcessExit::Unknown(format!("could not spawn powershell: {e}")),
        };
        // The receiver may already be dropped (caller gave up waiting) --
        // that's fine, this is a best-effort diagnostic, not load-bearing.
        let _ = tx.send(observation);
    });
    rx
}

/// Embedding a `tmux`-compatible binary into the daemon executable so users
/// never need to install it themselves (RAL-102 AC).
///
/// Windows lands first per the ticket, but no verified binary is bundled in
/// this build yet — the ticket itself calls out that the specific Windows
/// tmux build needs to be sourced deliberately (its `respawn-pane` gap alone
/// proves it isn't a drop-in of upstream tmux) and its licensing/static-vs-
/// dynamic linkage confirmed before distribution. Embedding is therefore
/// gated behind the `embedded-tmux` Cargo feature (default off): flip it on
/// only after placing a verified binary at
/// `daemon/assets/tmux/windows/tmux.exe` (see
/// `daemon/assets/tmux/windows/README.md` and `docs/tmux-embedding.md`) and
/// rebuilding. Without the feature (or on a platform with no embedded asset
/// yet), [`extract`] returns a clear error telling the operator to install
/// tmux or set [`TMUX_CMD_ENV`] — resolution never panics or silently no-ops.
mod embedded {
    use super::TmuxError;
    use std::path::PathBuf;

    #[cfg(all(target_os = "windows", feature = "embedded-tmux"))]
    static TMUX_EXE: &[u8] = include_bytes!("../assets/tmux/windows/tmux.exe");

    #[cfg(all(target_os = "windows", feature = "embedded-tmux"))]
    pub fn extract() -> Result<PathBuf, TmuxError> {
        let dir = std::env::temp_dir().join("ralphus-embedded-tmux");
        std::fs::create_dir_all(&dir)
            .map_err(|e| TmuxError(format!("could not create {}: {e}", dir.display())))?;
        let path = dir.join("tmux.exe");
        if !path.is_file() {
            std::fs::write(&path, TMUX_EXE)
                .map_err(|e| TmuxError(format!("could not write embedded tmux: {e}")))?;
        }
        Ok(path)
    }

    #[cfg(not(all(target_os = "windows", feature = "embedded-tmux")))]
    pub fn extract() -> Result<PathBuf, TmuxError> {
        Err(TmuxError(
            "tmux was not found on PATH, and this build has no embedded tmux binary \
             (RAL-102: embedding is gated behind the `embedded-tmux` feature and requires a \
             verified binary at daemon/assets/tmux/<platform>/ — see docs/tmux-embedding.md). \
             Install tmux and put it on PATH, or set RALPHUS_TMUX_CMD to its full path."
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    // Test harness output (`SKIP:` notices) legitimately goes to stdout so
    // `cargo test --nocapture` shows it; no JSON contract exists here.
    #![allow(clippy::print_stdout)]

    use super::*;

    #[test]
    fn tmux_history_limit_stays_above_the_runners_10k_capture_window() {
        // See `TMUX_HISTORY_LIMIT`'s doc comment: `runner.rs`'s poll loop and
        // reattach path read `capture_pane(..., 10_000)`, the deepest window
        // any consumer reads. Lowering the history-limit below that floor
        // without also lowering the capture window risks losing scrolled-past
        // events/done-sentinels. This is a change-detector by design: it
        // should force a deliberate look at `runner.rs`'s capture windows
        // whenever `TMUX_HISTORY_LIMIT` moves.
        const RUNNER_CAPTURE_WINDOW: u32 = 10_000;
        let limit: u32 = TMUX_HISTORY_LIMIT
            .parse()
            .expect("TMUX_HISTORY_LIMIT must be a valid tmux history-limit integer");
        assert!(
            limit >= RUNNER_CAPTURE_WINDOW,
            "TMUX_HISTORY_LIMIT ({limit}) must stay >= the runner's capture window \
             ({RUNNER_CAPTURE_WINDOW}) or the poll loop can miss scrolled-past \
             events/done-sentinels"
        );
    }

    #[test]
    fn split_command_separates_program_from_leading_args() {
        assert_eq!(
            split_command("wsl.exe tmux"),
            ("wsl.exe".to_string(), vec!["tmux".to_string()])
        );
        assert_eq!(
            split_command("tmux"),
            ("tmux".to_string(), Vec::<String>::new())
        );
        assert_eq!(
            split_command("C:\\Users\\me\\tmux.exe"),
            ("C:\\Users\\me\\tmux.exe".to_string(), Vec::<String>::new())
        );
    }

    #[test]
    fn tmux_from_program_exposes_program_and_prefix_args_separately() {
        let t = Tmux::from_program("wsl.exe tmux");
        assert_eq!(t.program(), "wsl.exe");
        assert_eq!(t.prefix_args(), ["tmux".to_string()]);

        let bare = Tmux::from_program("tmux");
        assert_eq!(bare.program(), "tmux");
        assert!(bare.prefix_args().is_empty());
    }

    #[test]
    fn session_name_is_deterministic_and_sanitized() {
        let a = session_name("run-1", "build", "session/with weird:chars");
        let b = session_name("run-1", "build", "session/with weird:chars");
        assert_eq!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        assert!(a.starts_with("ralphus_run-1_build_session"));
    }

    #[test]
    fn session_name_differs_for_different_sessions() {
        assert_ne!(
            session_name("run-1", "t", "a"),
            session_name("run-1", "t", "b")
        );
        assert_ne!(
            session_name("run-1", "t", "a"),
            session_name("run-2", "t", "a")
        );
    }

    #[test]
    fn session_name_differs_for_different_tasks_sharing_a_session_id() {
        // RAL-102 collision bug: sibling tasks that both use the conventional
        // session id "work" must not produce the same tmux session name.
        assert_ne!(
            session_name("run-1", "task-a", "work"),
            session_name("run-1", "task-b", "work"),
        );
    }

    #[test]
    fn session_name_truncates_long_ids() {
        let long_id = "x".repeat(500);
        let name = session_name("run-1", "build", &long_id);
        assert!(name.len() <= 200);
    }

    /// A throwaway directory for a pane-snapshot test, cleaned up on drop —
    /// keeps these tests off the real `state_dir()`.
    struct TempSnapshotDir(PathBuf);
    impl TempSnapshotDir {
        fn new(unique: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ralphus-test-pane-snapshots-{unique}"));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }
    }
    impl Drop for TempSnapshotDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn pane_snapshot_round_trips() {
        let dir = TempSnapshotDir::new("round-trip");
        write_pane_snapshot_in(&dir.0, "ralphus_run-1_build_work", "line one\nline two");
        assert_eq!(
            read_pane_snapshot_in(&dir.0, "ralphus_run-1_build_work"),
            Some("line one\nline two".to_string())
        );
    }

    #[test]
    fn pane_snapshot_redacts_registered_secret_values() {
        // RAL-264: the pane snapshot is the durable "what was last there"
        // record the board's Live View shows; a pane that echoed the agent's
        // own `$env:... = 'sk-or-v1-...'` assignment must not persist the
        // resolved value under it.
        crate::redact::with_registry_lock(|| {
            crate::redact::clear_for_tests();
            crate::redact::register("sk-or-v1-pane-snapshot-test-token");

            let dir = TempSnapshotDir::new("redact");
            write_pane_snapshot_in(
                &dir.0,
                "s",
                "$env:ANTHROPIC_AUTH_TOKEN = 'sk-or-v1-pane-snapshot-test-token'\ndone",
            );
            let content = read_pane_snapshot_in(&dir.0, "s").expect("snapshot written");
            assert!(
                !content.contains("sk-or-v1-pane-snapshot-test-token"),
                "secret leaked into pane snapshot: {content}"
            );
            assert!(content.contains(crate::redact::REDACTED));
            assert!(content.contains("done"));
        });
    }

    #[test]
    fn pane_snapshot_missing_file_is_none() {
        let dir = TempSnapshotDir::new("missing");
        assert_eq!(read_pane_snapshot_in(&dir.0, "never-written"), None);
    }

    #[test]
    fn write_pane_snapshot_redacts_secret_env_values_from_disk() {
        // RAL-247: the persisted pane-snapshot *file* must never contain a
        // credential env-var value, even when the captured pane did.
        let dir = TempSnapshotDir::new("redact-secret");
        let secret = "sk-snapshot-leak-guard";
        let content = format!("$env:ANTHROPIC_AUTH_TOKEN = '{secret}';\nordinary line");
        write_pane_snapshot_in(&dir.0, "s", &content);
        let disk =
            std::fs::read_to_string(pane_snapshot_path_in(&dir.0, "s")).expect("snapshot written");
        assert!(
            !disk.contains(secret),
            "credential value leaked into the pane-snapshot file: {disk}"
        );
        assert!(disk.contains("[REDACTED]"), "{disk}");
        assert!(
            disk.contains("ordinary line"),
            "non-secret content must be preserved: {disk}"
        );
    }

    #[test]
    fn read_pane_snapshot_redacts_secret_values_from_a_legacy_file() {
        // RAL-247: a snapshot file written before this fix may already carry
        // a secret value — the read path must scrub it defensively too.
        let dir = TempSnapshotDir::new("read-redacts-legacy");
        let secret = "sk-snapshot-legacy";
        let path = pane_snapshot_path_in(&dir.0, "s");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("$env:ANTHROPIC_API_KEY = '{secret}'")).unwrap();
        let content = read_pane_snapshot_in(&dir.0, "s").expect("legacy snapshot readable");
        assert!(
            !content.contains(secret),
            "credential value leaked from a legacy snapshot: {content}"
        );
        assert!(content.contains("[REDACTED]"), "{content}");
    }

    #[test]
    fn pane_snapshot_empty_content_reads_back_as_none() {
        // An attempt that produced no pane output at all writes an empty
        // file (see `write_pane_snapshot`'s doc comment) — the reader treats
        // that the same as "no snapshot", not a real-but-blank record, so
        // callers don't have to special-case an empty string.
        let dir = TempSnapshotDir::new("empty");
        write_pane_snapshot_in(&dir.0, "s", "");
        assert_eq!(read_pane_snapshot_in(&dir.0, "s"), None);
    }

    #[test]
    fn pane_snapshot_overwrites_a_prior_attempt() {
        let dir = TempSnapshotDir::new("overwrite");
        write_pane_snapshot_in(&dir.0, "s", "first attempt's output");
        write_pane_snapshot_in(&dir.0, "s", "second attempt's output");
        assert_eq!(
            read_pane_snapshot_in(&dir.0, "s"),
            Some("second attempt's output".to_string())
        );
    }

    #[test]
    fn pane_snapshot_is_truncated_to_the_line_limit() {
        let dir = TempSnapshotDir::new("truncate");
        let lines: Vec<String> = (0..PANE_SNAPSHOT_MAX_LINES + 500)
            .map(|i| format!("line {i}"))
            .collect();
        write_pane_snapshot_in(&dir.0, "s", &lines.join("\n"));
        let saved = read_pane_snapshot_in(&dir.0, "s").expect("snapshot written");
        assert_eq!(saved.lines().count(), PANE_SNAPSHOT_MAX_LINES);
        // Keeps the *last* lines (the most recent output), not the first.
        assert!(saved.ends_with(&format!("line {}", PANE_SNAPSHOT_MAX_LINES + 499)));
    }

    #[test]
    fn trim_trailing_blank_pane_lines_strips_padding_rows() {
        assert_eq!(
            trim_trailing_blank_pane_lines("line1\nline2\nline3\n\n\n\n\n"),
            "line1\nline2\nline3"
        );
    }

    #[test]
    fn trim_trailing_blank_pane_lines_leaves_a_full_pane_unchanged() {
        // No blank tail (content spans the whole pane height) -- only the
        // single natural trailing newline is dropped, no real content lost.
        assert_eq!(
            trim_trailing_blank_pane_lines("line1\nline2\nline3\n"),
            "line1\nline2\nline3"
        );
    }

    #[test]
    fn trim_trailing_blank_pane_lines_preserves_interior_blank_lines() {
        assert_eq!(
            trim_trailing_blank_pane_lines("line1\n\nline2\n\n\n"),
            "line1\n\nline2"
        );
    }

    #[test]
    fn trim_trailing_blank_pane_lines_all_blank_becomes_empty() {
        assert_eq!(trim_trailing_blank_pane_lines("\n\n\n\n"), "");
    }

    #[test]
    fn pane_snapshot_different_sessions_do_not_collide() {
        let dir = TempSnapshotDir::new("distinct");
        write_pane_snapshot_in(&dir.0, "session-a", "a's output");
        write_pane_snapshot_in(&dir.0, "session-b", "b's output");
        assert_eq!(
            read_pane_snapshot_in(&dir.0, "session-a"),
            Some("a's output".to_string())
        );
        assert_eq!(
            read_pane_snapshot_in(&dir.0, "session-b"),
            Some("b's output".to_string())
        );
    }

    #[test]
    fn quote_for_shell_escapes_single_quotes() {
        let quoted = quote_for_shell("it's a test");
        assert!(quoted.contains("it"));
        assert!(quoted.starts_with('\''));
        assert!(quoted.ends_with('\''));
    }

    #[test]
    fn build_command_line_quotes_every_argument() {
        let line = build_command_line("prog", &["a b".to_string(), "c".to_string()]);
        assert!(line.contains("'prog' "));
        assert!(line.contains("'a b'"));
        assert!(line.contains("'c'"));
        if cfg!(target_os = "windows") {
            assert!(
                line.starts_with("& "),
                "Windows needs the PowerShell call operator to invoke a quoted command name: {line}"
            );
        } else {
            assert!(line.starts_with("'prog' "));
        }
    }

    #[test]
    fn build_command_line_with_env_empty_matches_plain() {
        let line = build_command_line_with_env("prog", &["a".to_string()], &BTreeMap::new());
        assert_eq!(line, build_command_line("prog", &["a".to_string()]));
    }

    #[test]
    fn build_command_line_with_env_prefixes_assignments() {
        let mut env = BTreeMap::new();
        env.insert("A".to_string(), "1".to_string());
        env.insert("B".to_string(), "it's".to_string());
        let line = build_command_line_with_env("prog", &[], &env);
        if cfg!(target_os = "windows") {
            assert!(line.starts_with("$env:A = '1'; $env:B = "));
            assert!(line.contains("& 'prog'"));
        } else {
            assert!(line.starts_with("A='1' B="));
            assert!(line.ends_with("'prog'"));
        }
    }

    #[test]
    fn build_command_line_with_env_drops_invalid_keys() {
        let mut env = BTreeMap::new();
        env.insert("$(bad)".to_string(), "x".to_string());
        let line = build_command_line_with_env("prog", &[], &env);
        assert_eq!(line, build_command_line("prog", &[]));
    }

    fn tmux_on_path() -> bool {
        find_on_path("tmux").is_some()
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_new_session_capture_and_kill_roundtrip() {
        if !tmux_on_path() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        let _guard = LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmux = Tmux::resolve().unwrap();
        let name = session_name(&unique_test_tag("test-run"), "build", "roundtrip");
        let _ = tmux.kill_session(&name);

        let cwd = std::env::temp_dir();
        let cwd_str = cwd.to_string_lossy().into_owned();
        tmux.new_detached_session_with_command(
            &name,
            &cwd_str,
            &BTreeMap::new(),
            "echo",
            &["tmux-roundtrip-ok".to_string()],
        )
        .unwrap();

        // `cargo test`'s default harness runs hundreds of tests concurrently
        // across a CI runner's small core count, so this real tmux
        // subprocess (session start + respawn-pane + first successful
        // capture) can take much longer to be scheduled than on a
        // lightly-loaded dev machine -- 150 polls/30s gives real headroom
        // under that contention without slowing the common case, since the
        // loop still breaks the moment the marker appears.
        let mut seen = String::new();
        for _ in 0..150 {
            std::thread::sleep(Duration::from_millis(200));
            seen = tmux.capture_pane(&name, 50).unwrap_or_default();
            if seen.contains("tmux-roundtrip-ok") {
                break;
            }
        }
        assert!(
            seen.contains("tmux-roundtrip-ok"),
            "expected captured pane to contain the echoed marker, got: {seen:?}"
        );

        tmux.kill_session(&name).unwrap();
        assert!(!tmux.has_session(&name));
        // Killing an already-gone session is a no-op success, not an error.
        tmux.kill_session(&name).unwrap();
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_capture_pane_has_no_trailing_blank_lines_for_short_output() {
        // RAL-237: a session whose real output is far shorter than the
        // pane's height (`-y 50`) must not come back with genuine trailing
        // blank rows -- that's what made the Live View's jump-to-latest
        // button scroll past the real content into empty space.
        if !tmux_on_path() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        let _guard = LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmux = Tmux::resolve().unwrap();
        let name = session_name(&unique_test_tag("test-run"), "build", "shortoutput");
        let _ = tmux.kill_session(&name);

        let cwd = std::env::temp_dir();
        let cwd_str = cwd.to_string_lossy().into_owned();
        tmux.new_detached_session_with_command(
            &name,
            &cwd_str,
            &BTreeMap::new(),
            "echo",
            &["ral237-marker".to_string()],
        )
        .unwrap();

        let mut captured = String::new();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            captured = tmux.capture_pane(&name, 50).unwrap_or_default();
            if captured.contains("ral237-marker") {
                break;
            }
        }
        assert!(
            captured.contains("ral237-marker"),
            "expected captured pane to contain the echoed marker, got: {captured:?}"
        );
        assert!(
            !captured.ends_with('\n') && !captured.ends_with("\n\n"),
            "expected no trailing blank lines after the real content, got: {captured:?}"
        );
        let lines: Vec<&str> = captured.lines().collect();
        let last = *lines.last().expect("captured pane has at least one line");
        assert!(
            !last.trim().is_empty(),
            "last captured line should be real content, not blank padding: {captured:?}"
        );

        tmux.kill_session(&name).unwrap();
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_has_session_false_for_unknown_name() {
        if !tmux_on_path() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        let tmux = Tmux::resolve().unwrap();
        assert!(!tmux.has_session("definitely-not-a-real-ralphus-session-xyz"));
    }

    /// RAL-247: on Windows the per-session env overrides are delivered via
    /// `new-session -e KEY=value` (see [`Tmux::new_detached_session_with_command`]),
    /// not by inlining `$env:KEY = 'value'` assignments into the `send-keys`
    /// line, so a value that would previously have been typed — and echoed —
    /// into the pane never becomes pane text. This test drives a sentinel
    /// token through that real delivery path and asserts the pane shows the
    /// command but never the value, while the process still sees the variable
    /// (proved by echoing its length, never the value).
    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_env_override_delivered_without_leaking_its_value() {
        if !tmux_on_path() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        if !cfg!(target_os = "windows") {
            println!("SKIP: exercises the Windows send-keys delivery path only");
            return;
        }
        let _guard = LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let sentinel = "sk-ant-test-247-leak-guard";
        let mut env = BTreeMap::new();
        env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), sentinel.to_string());

        let tmux = Tmux::resolve().unwrap();
        let name = session_name(&unique_test_tag("test-run"), "build", "env-redaction");
        let _ = tmux.kill_session(&name);
        let cwd = std::env::temp_dir();
        let cwd_str = cwd.to_string_lossy().into_owned();
        tmux.new_detached_session_with_command(
            &name,
            &cwd_str,
            &env,
            "echo",
            &["intended-marker".to_string()],
        )
        .unwrap();

        let mut seen = String::new();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            seen = tmux.capture_pane(&name, 50).unwrap_or_default();
            if seen.contains("intended-marker") {
                break;
            }
        }
        assert!(
            seen.contains("intended-marker"),
            "expected the launch command to run, pane: {seen:?}"
        );
        // The credential must still be present in the pane's process env:
        // echo `$env:ANTHROPIC_AUTH_TOKEN.Length` (a number, not the value).
        tmux.send_keys_literal(
            &name,
            "Write-Output (\":LEN=\" + $env:ANTHROPIC_AUTH_TOKEN.Length)",
        )
        .unwrap();
        let mut with_len = String::new();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            with_len = tmux.capture_pane(&name, 50).unwrap_or_default();
            if with_len.contains(":LEN=") {
                break;
            }
        }
        tmux.kill_session(&name).unwrap();

        // The credential reached the process...
        assert!(
            with_len.contains(&format!(":LEN={}", sentinel.len())),
            "expected the sentinel length to be echoed (credential was not \
             delivered), pane: {with_len:?}"
        );
        // ...but its value never appeared as pane text.
        assert!(
            !with_len.contains(sentinel),
            "sentinel value leaked into the pane: {with_len:?}"
        );
        assert!(
            !with_len.contains("$env:ANTHROPIC_AUTH_TOKEN ="),
            "no inline assignment should have been typed: {with_len:?}"
        );
    }

    /// RAL-227: proves *why* `crate::config::is_valid_env_value` rejects a
    /// `\n`-carrying env-override value at the HTTP boundary
    /// (`daemon/src/server.rs`). Since RAL-247, Windows delivers overrides via
    /// `new-session -e` (never typed into the pane), so a would-be injected
    /// `\n...` in a value is carried as literal env data, not re-interpreted
    /// as a second command — the intended marker still runs. This test
    /// bypasses the boundary check on purpose and drives such a value down the
    /// real delivery path to confirm the launch line is no longer a place a
    /// newline can corrupt or split a command.
    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_env_value_newline_delivered_via_e_does_not_corrupt_the_command() {
        if !tmux_on_path() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        if !cfg!(target_os = "windows") {
            println!("SKIP: exercises the Windows send-keys delivery path only");
            return;
        }
        let _guard = LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let malicious_value = "first\necho SPLIT-MARKER";
        assert!(
            !crate::config::is_valid_env_value(malicious_value),
            "the boundary check must reject an embedded-newline value"
        );

        let mut env = BTreeMap::new();
        env.insert("INJECTED".to_string(), malicious_value.to_string());

        let tmux = Tmux::resolve().unwrap();
        let name = session_name(&unique_test_tag("test-run"), "build", "newline-injection");
        let _ = tmux.kill_session(&name);
        let cwd = std::env::temp_dir();
        let cwd_str = cwd.to_string_lossy().into_owned();
        tmux.new_detached_session_with_command(
            &name,
            &cwd_str,
            &env,
            "echo",
            &["intended-marker".to_string()],
        )
        .unwrap();

        let mut seen = String::new();
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(200));
            seen = tmux.capture_pane(&name, 50).unwrap_or_default();
            if seen.contains("intended-marker") {
                break;
            }
        }
        tmux.kill_session(&name).unwrap();

        assert!(
            seen.contains("intended-marker"),
            "expected the intended command to run despite the newline value, \
             pane: {seen:?}"
        );
        assert!(
            !seen.contains("SPLIT-MARKER"),
            "the newline value must not be executed as a second command, \
             pane: {seen:?}"
        );
    }
}
