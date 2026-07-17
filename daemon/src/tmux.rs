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

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Overrides tmux resolution entirely — set to the full path (or bare name,
/// if it's on `PATH` under a different name) of the tmux-compatible binary to
/// use, skipping both the `PATH` lookup and the embedded fallback.
pub const TMUX_CMD_ENV: &str = "RALPHUS_TMUX_CMD";

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
    /// treated as "nothing to kill", not an error.
    pub fn kill_sessions_with_prefix(&self, prefix: &str) {
        for name in self.list_sessions_with_prefix(prefix).unwrap_or_default() {
            let _ = self.kill_session(&name);
        }
    }

    /// Create a detached session named `name` rooted at `cwd`, with
    /// `remain-on-exit` enabled, then start `command` as the pane's process.
    ///
    /// Real tmux supports replacing the initial shell in one step
    /// (`respawn-pane <command>`); the Windows-alternative tmux build this
    /// project targets does not execute a command argument passed to
    /// `respawn-pane` (verified empirically — the call succeeds but the shell
    /// prompt is left running), so on Windows the command is instead typed
    /// into the shell via `send-keys` — the same fallback gastown's own
    /// Windows port uses for the identical gap.
    ///
    /// # Errors
    /// Returns an error if any of the underlying tmux calls fail; the
    /// partially-created session is killed before returning so a failed
    /// start never leaks a zombie session.
    pub fn new_detached_session_with_command(
        &self,
        name: &str,
        cwd: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        // Deliberately very wide (default is much narrower) so a long line --
        // e.g. a Bash tool call's rendered command/description in the live
        // pane -- doesn't get hard-wrapped by the pane itself on top of the
        // runner's own 80-char truncation, which made captured output nearly
        // unreadable (two independent truncations stacking). No real
        // downside to going wide here: this pane is consumed via
        // `capture-pane` (an automated poll), not sat in front of by a human
        // at a fixed terminal width, so there's no reason to economize.
        self.run(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            cwd,
            "-x",
            "500",
            "-y",
            "50",
        ])?;
        // Best-effort: without this, tmux discards a dead pane's content
        // immediately, which would race the daemon's own sentinel-based
        // completion detection.
        let _ = self.run(&["set-option", "-t", name, "remain-on-exit", "on"]);
        // Best-effort: raise the scrollback limit well past tmux's default
        // (2000 lines) so `ralphus history --live` (RAL-140), which polls
        // `capture-pane` with a much larger `lines` window than the board's
        // live-view default, doesn't silently lose older output to tmux's
        // own buffer trimming while a long-running session is still live.
        let _ = self.run(&["set-option", "-t", name, "history-limit", "200000"]);
        let started = if cfg!(target_os = "windows") {
            self.run(&["send-keys", "-t", name, command, "Enter"])
        } else {
            self.run(&["respawn-pane", "-k", "-t", name, "-c", cwd, command])
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
    /// # Errors
    /// Returns an error if the session does not exist or tmux fails.
    pub fn capture_pane(&self, name: &str, lines: u32) -> Result<String, TmuxError> {
        self.run(&["capture-pane", "-p", "-t", name, "-S", &format!("-{lines}")])
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
        // `kill-session` on the Windows tmux-alternative this project targets
        // (psmux) frees the *name* from its own registry but never actually
        // terminates the backing OS process (confirmed: see
        // "BREAKTHROUGH: kill-session leaks the underlying OS process" in
        // PSMUX_CRASH_NOTES.local.md -- sessions explicitly killed hours
        // earlier were still alive as real `tmux.exe` processes, spinning
        // CPU). `has_session(name)` is confirmed false above, so any
        // `tmux.exe` still alive under this exact deterministic session name
        // is unambiguously a zombie, never a live session we might still
        // need -- safe to force-terminate directly.
        let killed = force_kill_tmux_processes(name);
        if killed > 0 {
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
fn force_kill_tmux_processes(needle: &str) -> usize {
    if !cfg!(target_os = "windows") {
        return 0;
    }
    // `needle` is always either `tmux::session_name`'s output (already
    // sanitized to `[a-zA-Z0-9_-]`) or the literal `"ralphus_"` prefix --
    // safe to interpolate into the PowerShell literal below, same
    // reasoning as `find_server_pid`'s identical pattern.
    let script = format!(
        "$procs = Get-CimInstance Win32_Process -Filter \"Name='tmux.exe'\" -ErrorAction SilentlyContinue | \
         Where-Object {{ $_.CommandLine -like '*{needle}*' -and $_.CommandLine -like '*server*' }}; \
         $procs | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}; \
         $procs.Count"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0),
        _ => 0,
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
    // `name` is always `tmux::session_name`'s output, already sanitized to
    // `[a-zA-Z0-9_-]` -- safe to interpolate into the PowerShell literal
    // below with no injection risk.
    let script = format!(
        "(Get-CimInstance Win32_Process -Filter \"Name='tmux.exe'\" -ErrorAction SilentlyContinue | \
         Where-Object {{ $_.CommandLine -like '*{name}*' -and $_.CommandLine -like '*server*' }} | \
         Select-Object -First 1 -ExpandProperty ProcessId)"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pid: Option<u32> = String::from_utf8_lossy(&output.stdout).trim().parse().ok();
    // Logged unconditionally (not just on the eventual failure path) so a
    // real occurrence always leaves a record of which PID was tracked --
    // useful for cross-referencing manually (e.g. a live process-watch
    // script) even when `watch_for_exit`'s own diagnostic later comes back
    // `Unknown`.
    match pid {
        Some(p) => crate::rlog!(
            DEBUG,
            "ralphus [runner] found tmux server pid={p} for session={name}"
        ),
        None => crate::rlog!(
            DEBUG,
            "ralphus [runner] could not find tmux server pid for session={name} (no matching process)"
        ),
    }
    pid
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
    use super::*;

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

    fn tmux_on_path() -> bool {
        find_on_path("tmux").is_some()
    }

    #[test]
    fn live_tmux_new_session_capture_and_kill_roundtrip() {
        if !tmux_on_path() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        let tmux = Tmux::resolve().unwrap();
        let name = session_name("test-run", "build", "roundtrip");
        let _ = tmux.kill_session(&name);

        let cwd = std::env::temp_dir();
        let cwd_str = cwd.to_string_lossy().into_owned();
        let command = build_command_line("echo", &["tmux-roundtrip-ok".to_string()]);
        tmux.new_detached_session_with_command(&name, &cwd_str, &command)
            .unwrap();

        let mut seen = String::new();
        for _ in 0..50 {
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

    #[test]
    fn live_tmux_has_session_false_for_unknown_name() {
        if !tmux_on_path() {
            println!("SKIP: tmux not found on PATH");
            return;
        }
        let tmux = Tmux::resolve().unwrap();
        assert!(!tmux.has_session("definitely-not-a-real-ralphus-session-xyz"));
    }
}
