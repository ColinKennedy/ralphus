//! Direct (tmux-free) cell execution.
//!
//! The runner binary (`ralphus-runner send`) is headless: it drives the agent
//! over piped stdio, prints `RALPHUS_EVENT:` lines on its own stdout and
//! writes a result file. Hosting it in a tmux pane only gives the board
//! something to look at, so this path spawns it as a plain child process and
//! tees its stdout and stderr into the same durable `.raw` transcript a
//! `pipe-pane` sink would write. Everything downstream -- event forwarding,
//! stall detection, terminal logs, the Live View transcript endpoints -- reads
//! that file and is unchanged.
//!
//! Differences from the tmux path ([`SubprocessRunner::run_via_tmux`]):
//! - completion is the child's exit status plus its result file, not a
//!   sentinel line in a pane;
//! - there is no pane to vanish, so no reattach loop;
//! - cancellation kills the child's whole process tree
//!   ([`crate::proof::ProcessTree`]);
//! - Live View of a running cell is the transcript tail
//!   ([`live_tail`]), not a rendered screen.
//!
//! `RALPHUS_RUNNER_MODE` selects the path: `tmux` forces the old path,
//! `direct` forces this one, and unset lets [`use_direct`] decide.

use super::*;
use std::collections::HashSet;
use std::io::{Read as _, Write as _};
use std::sync::{Arc, LazyLock, Mutex};

/// How often the attempt loop checks the child's exit status and the stop
/// conditions. A `try_wait` is a syscall, not a process, so this is cheap.
const DIRECT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Longest the run waits for the output pumps to finish after the child has
/// exited. A grandchild that outlived the child can hold the pipes open
/// indefinitely, and its output is not worth blocking the cell on.
const PUMP_JOIN_GRACE: Duration = Duration::from_secs(2);

/// How many trailing transcript lines the ended-cell snapshot keeps.
const SNAPSHOT_TAIL_LINES: usize = 200;

/// Which execution path a spec takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunnerMode {
    /// `RALPHUS_RUNNER_MODE=tmux`: every spec runs in a tmux pane.
    Tmux,
    /// `RALPHUS_RUNNER_MODE=direct`: every spec runs as a plain child.
    Direct,
    /// Unset or unrecognized: [`use_direct`] picks per spec.
    Auto,
}

fn runner_mode() -> RunnerMode {
    match std::env::var("RALPHUS_RUNNER_MODE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "tmux" => RunnerMode::Tmux,
        "direct" => RunnerMode::Direct,
        _ => RunnerMode::Auto,
    }
}

/// Whether `spec` runs as a plain child process rather than in a tmux pane.
pub(super) fn use_direct(spec: &RunnerSpec) -> bool {
    match runner_mode() {
        RunnerMode::Tmux => false,
        RunnerMode::Direct => true,
        // `command` cells and proof steps have no agent to take over, so the
        // pane only ever served as a place to watch them from.
        RunnerMode::Auto => spec.prompt.is_none() && spec.command.is_some(),
    }
}

/// Session names (see [`crate::tmux::session_name`]) of cells currently
/// running through this path, so the Live View endpoints can tell "running
/// headless" from "no such session".
fn active() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    &ACTIVE
}

/// Whether `session_name` names a cell currently running without tmux.
#[must_use]
pub(crate) fn is_active(session_name: &str) -> bool {
    active()
        .lock()
        .is_ok_and(|names| names.contains(session_name))
}

/// Marks a cell active for as long as it lives.
struct ActiveGuard(String);

impl ActiveGuard {
    fn new(session_name: &str) -> Self {
        if let Ok(mut names) = active().lock() {
            names.insert(session_name.to_string());
        }
        Self(session_name.to_string())
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if let Ok(mut names) = active().lock() {
            names.remove(&self.0);
        }
    }
}

/// The last `lines` non-empty lines of `session_name`'s newest attempt
/// transcript, ANSI-stripped -- what the board shows for a running headless
/// cell. `None` when no transcript exists yet.
#[must_use]
pub(crate) fn live_tail(session_name: &str, lines: usize) -> Option<String> {
    let attempt = crate::terminal_log::latest_attempt(session_name).unwrap_or(0);
    transcript_tail(
        &crate::terminal_log::raw_transcript_path(session_name, attempt),
        lines,
    )
}

/// Reads at most the last 256 KiB of `path` and returns its trailing `lines`.
fn transcript_tail(path: &std::path::Path, lines: usize) -> Option<String> {
    use std::io::{Seek as _, SeekFrom};
    const WINDOW: u64 = 256 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(WINDOW)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = crate::terminal_log::strip_ansi_escapes(&String::from_utf8_lossy(&bytes));
    Some(tail_lines(&text.replace('\r', ""), lines))
}

/// The transcript file shared by a child's stdout and stderr pumps. Mirrors
/// `ralphus-runner pipe-sink`: appends up to `cap` bytes, then drops the rest
/// and writes one truncation marker so a reader knows the record is partial.
struct CappedSink {
    file: std::fs::File,
    written: u64,
    cap: u64,
    marked: bool,
}

impl CappedSink {
    fn write(&mut self, bytes: &[u8]) {
        if self.written < self.cap {
            let take = usize::try_from(self.cap - self.written)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            if self.file.write_all(&bytes[..take]).is_err() {
                return;
            }
            self.written += take as u64;
        }
        if self.written >= self.cap && !self.marked {
            self.marked = true;
            let _ = writeln!(
                self.file,
                "\n[ralphus-runner: transcript truncated at {} bytes]",
                self.cap
            );
        }
        let _ = self.file.flush();
    }
}

/// The shell the runner should treat as "the shell that launched it".
///
/// The runner runs a `command` with the nearest shell among its ancestor
/// processes. In a tmux pane that was always the pane's own shell; as a direct
/// child of the daemon it would instead be whatever shell happened to start
/// the daemon. On Windows the pane was an interactive PowerShell, so this
/// pins that. `None` leaves detection alone: on POSIX the pane shell was the
/// user's login shell, which is what ancestry finds anyway, and an explicit
/// `RALPHUS_SHELL` always wins.
fn pane_equivalent_shell(
    env_overrides: &std::collections::BTreeMap<String, String>,
) -> Option<&'static str> {
    if !cfg!(windows)
        || std::env::var_os("RALPHUS_SHELL").is_some()
        || env_overrides.contains_key("RALPHUS_SHELL")
    {
        return None;
    }
    Some(
        if crate::agent_profiles::resolve_executable("pwsh").is_ok() {
            "pwsh"
        } else {
            "powershell"
        },
    )
}

/// Copies `source` into `sink` until EOF.
fn pump(
    mut source: impl std::io::Read + Send + 'static,
    sink: Arc<Mutex<CappedSink>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match source.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut sink) = sink.lock() {
                        sink.write(&buf[..n]);
                    }
                }
            }
        }
    })
}

/// Waits up to [`PUMP_JOIN_GRACE`] for the pump threads to finish.
fn join_pumps(pumps: Vec<std::thread::JoinHandle<()>>) {
    let until = Instant::now() + PUMP_JOIN_GRACE;
    for handle in pumps {
        while !handle.is_finished() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

impl SubprocessRunner {
    /// Run `spec` as a plain child process. See the module docs.
    pub(super) fn run_direct(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        let io_dir = std::env::temp_dir().join("ralphus-runner-io");
        if let Err(e) = std::fs::create_dir_all(&io_dir) {
            return RunnerResult::failure(format!("could not create runner IO dir: {e}"));
        }
        // Same deterministic name the tmux path uses, so the board's
        // per-cell endpoints and the transcript/log paths line up.
        let session_name = crate::tmux::session_name(&spec.squad_id, &spec.task, &spec.cell_id);
        let spec_path = io_dir.join(format!("{session_name}.spec.json"));
        let result_path = io_dir.join(format!("{session_name}.result.json"));
        let _active = ActiveGuard::new(&session_name);

        let detach_token = self.detachments.as_ref().map(|d| d.register(&session_name));
        let _detach_guard = self.detachments.as_ref().map(|d| DetachGuard {
            detachments: d,
            session_name: &session_name,
        });
        let waypoint_halt_token = self
            .waypoint_halts
            .as_ref()
            .map(|w| w.register(&spec.squad_id));
        let _waypoint_halt_guard = self.waypoint_halts.as_ref().map(|w| WaypointHaltGuard {
            waypoint_halts: w,
            squad_id: &spec.squad_id,
        });
        let arbiter_key = crate::store_memory::StoreMemory::cell_diff_key(
            &spec.squad_id,
            &spec.task,
            &spec.cell_id,
        );
        let arbiter_stop_token = self
            .arbiter_stops
            .as_ref()
            .map(|s| s.register(&arbiter_key));
        let _arbiter_stop_guard = self.arbiter_stops.as_ref().map(|s| ArbiterStopGuard {
            stops: s,
            key: &arbiter_key,
        });

        let started = Instant::now();
        let deadline = spec.timeout_sec.map(Duration::from_secs);
        let result = self.run_direct_attempt(
            spec,
            cancel,
            detach_token.as_ref(),
            waypoint_halt_token.as_ref(),
            arbiter_stop_token.as_ref(),
            &session_name,
            &spec_path,
            &result_path,
            &started,
            deadline,
        );
        crate::rlog!(
            INFO,
            "ralphus [runner] direct done squad={} cell={} status={} tokens_in={} tokens_out={} cost_usd={:.4}",
            spec.squad_id,
            spec.cell_id,
            result.status,
            result.tokens_in,
            result.tokens_out,
            result.cost_usd,
        );
        self.emit_direct_note(spec, "direct run finished", &session_name);
        self.clear_live_activity(&session_name);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn run_direct_attempt(
        &self,
        spec: &RunnerSpec,
        cancel: &CancelToken,
        detach: Option<&crate::cancel::DetachToken>,
        waypoint_halt: Option<&crate::cancel::WaypointHaltToken>,
        arbiter_stop: Option<&crate::cancel::ArbiterStopToken>,
        session_name: &str,
        spec_path: &std::path::Path,
        result_path: &std::path::Path,
        started: &Instant,
        deadline: Option<Duration>,
    ) -> RunnerResult {
        let _ = std::fs::remove_file(result_path);
        let payload = match serde_json::to_string(spec) {
            Ok(p) => p,
            Err(e) => return RunnerResult::failure(format!("could not serialize spec: {e}")),
        };
        if let Err(e) = std::fs::write(spec_path, payload) {
            return RunnerResult::failure(format!("could not write spec file: {e}"));
        }

        // The transcript path is keyed on (session, attempt), and every run
        // of a cell is attempt 0 here, so clear what a previous run left.
        let attempt = 0;
        let transcript_path = crate::terminal_log::raw_transcript_path(session_name, attempt);
        if let Some(parent) = transcript_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(&transcript_path);
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&transcript_path)
        {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_file(spec_path);
                return RunnerResult::failure(format!("could not open transcript: {e}"));
            }
        };
        let cap = crate::config::load_terminal_log_config().max_transcript_bytes_per_attempt();
        let sink = Arc::new(Mutex::new(CappedSink {
            file,
            written: 0,
            cap: if cap == 0 { u64::MAX } else { cap },
            marked: false,
        }));

        let mut command = std::process::Command::new(&self.program);
        command
            .args(&self.args)
            .arg("send")
            .arg(spec_path)
            .arg("--result-file")
            .arg(result_path)
            .current_dir(&spec.cwd)
            .envs(&spec.env_overrides)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(shell) = pane_equivalent_shell(&spec.env_overrides) {
            command.env("RALPHUS_SHELL", shell);
        }
        crate::proof::prepare_command(&mut command);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // The daemon has no console of its own to share, and a child
            // without this flag can pop a console window per cell.
            command.creation_flags(0x0800_0000);
        }
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(spec_path);
                return RunnerResult::failure(format!("could not start {}: {e}", self.program));
            }
        };
        let mut tree = crate::proof::ProcessTree::confine(&child);
        let mut pumps = Vec::new();
        if let Some(out) = child.stdout.take() {
            pumps.push(pump(out, Arc::clone(&sink)));
        }
        if let Some(err) = child.stderr.take() {
            pumps.push(pump(err, Arc::clone(&sink)));
        }
        crate::rlog!(
            INFO,
            "ralphus [runner] direct run started squad={} cell={} agent={} pid={}",
            spec.squad_id,
            spec.cell_id,
            spec.agent,
            child.id(),
        );
        self.emit_direct_note(spec, "direct run started", session_name);
        let _pid_guard = self.registry.as_ref().map(|registry| {
            registry.register(&spec.squad_id, &spec.cell_id, child.id());
            PidGuard {
                registry,
                squad_id: &spec.squad_id,
                cell_id: &spec.cell_id,
            }
        });

        let attempt_started_ms = crate::store::now_ms();
        let stall_threshold = mailbox_stall_threshold();
        let mut tailer = TranscriptTailer::new(session_name, attempt, 0);
        let agent_health_config = crate::config::load_agent_health_config();
        let mut stall_detector = self
            .thinking_capable_for_spec(spec)
            .then(|| crate::thinking_stall::ThinkingStallDetector::new(&agent_health_config));
        let mut resumable_agent_session_id: Option<String> = None;
        let mut current_usage = LiveUsage::default();
        let drain_interval = spec
            .maximum_budget_usd
            .map(|_| crate::config::load_budget_config().poll_interval())
            .unwrap_or(TMUX_POLL_INTERVAL);
        let mut last_drain = Instant::now();
        let mut first_tick = true;

        let target = || TranscriptEventTarget {
            pane_event_fallback: false,
            cartographer: self.cartographer.as_ref(),
            squad_id: &spec.squad_id,
            cell_id: &spec.cell_id,
            task: &spec.task,
            cwd: &spec.cwd,
            context: spec
                .prompt
                .as_deref()
                .unwrap_or(spec.command.as_deref().unwrap_or("")),
            arbiter_stops: self.arbiter_stops.as_ref(),
        };

        // `Some(result)` once the loop decides the cell's outcome without the
        // child having finished on its own; `None` when it exited by itself.
        let mut decided: Option<RunnerResult> = None;
        let mut exited = false;
        while decided.is_none() && !exited {
            let note = |message: &str| self.emit_direct_note(spec, message, session_name);
            if cancel.is_cancelled() {
                tree.kill(&mut child);
                crate::rlog!(
                    INFO,
                    "ralphus [runner] cancelled squad={} cell={}",
                    spec.squad_id,
                    spec.cell_id
                );
                note("direct run killed: cancelled");
                decided = Some(RunnerResult::failure("cancelled"));
            } else if detach.is_some_and(crate::cancel::DetachToken::is_cancelled) {
                tree.kill(&mut child);
                note("direct run killed: detached for manual takeover");
                decided = Some(RunnerResult::detached(
                    current_usage,
                    resumable_agent_session_id.clone(),
                ));
            } else if waypoint_halt.is_some_and(crate::cancel::WaypointHaltToken::is_cancelled) {
                tree.kill(&mut child);
                note("direct run killed: waypoint halt");
                decided = Some(RunnerResult::waypoint_halted(
                    current_usage,
                    resumable_agent_session_id.clone(),
                ));
            } else if arbiter_stop.is_some_and(crate::cancel::ArbiterStopToken::is_cancelled) {
                tree.kill(&mut child);
                let numstat = self
                    .cartographer
                    .as_ref()
                    .and_then(|store| {
                        store
                            .lock()
                            .memory()
                            .cell_diff_state(&crate::store_memory::StoreMemory::cell_diff_key(
                                &spec.squad_id,
                                &spec.task,
                                &spec.cell_id,
                            ))
                            .map(|state| state.summary)
                    })
                    .unwrap_or_default();
                note("direct run killed: stopped by Arbiter");
                decided = Some(RunnerResult::arbiter_stopped(
                    current_usage,
                    format!(
                        "terminated by Arbiter after suspicious large diff; numstat: {numstat}"
                    ),
                ));
            } else if timed_out(started.elapsed(), deadline) {
                tree.kill(&mut child);
                let secs = deadline.map_or(0, |d| d.as_secs());
                crate::rlog!(
                    WARNING,
                    "ralphus [runner] timed out after {secs}s squad={} cell={}",
                    spec.squad_id,
                    spec.cell_id
                );
                note("direct run killed: timed out");
                decided = Some(RunnerResult::failure(format!("timed out after {secs}s")));
            } else if spec
                .maximum_budget_usd
                .is_some_and(|cap| current_usage.cost_usd > cap)
            {
                tree.kill(&mut child);
                let cap = spec.maximum_budget_usd.unwrap_or_default();
                crate::rlog!(
                    WARNING,
                    "ralphus [runner] cost ${:.4} exceeded maximum_budget_usd cap ${cap:.4}, killing squad={} cell={}",
                    current_usage.cost_usd,
                    spec.squad_id,
                    spec.cell_id
                );
                note("direct run killed: cost limit exceeded");
                decided = Some(RunnerResult::cost_exceeded(current_usage, cap));
            } else if let Some(reason) = self.maximum_timeout_exceeded(spec, started.elapsed()) {
                tree.kill(&mut child);
                crate::rlog!(
                    WARNING,
                    "ralphus [runner] {reason}, killing squad={} cell={}",
                    spec.squad_id,
                    spec.cell_id
                );
                note("direct run killed: maximum_timeout_seconds exceeded");
                decided = Some(RunnerResult::maximum_timeout_exceeded(
                    current_usage,
                    reason,
                ));
            } else if matches!(child.try_wait(), Ok(Some(_))) {
                exited = true;
            } else if first_tick || last_drain.elapsed() >= drain_interval {
                first_tick = false;
                last_drain = Instant::now();
                self.check_stall_escalation(
                    spec,
                    session_name,
                    attempt_started_ms,
                    stall_threshold,
                );
                let drained = tailer.drain();
                if drained.saw_new_bytes {
                    self.note_live_activity(session_name);
                }
                let (_done, stall_sample) = consume_transcript_lines(
                    &drained.lines,
                    &target(),
                    &mut resumable_agent_session_id,
                    &mut current_usage,
                    stall_detector.as_mut(),
                );
                if let Some(sample) = stall_sample {
                    tree.kill(&mut child);
                    crate::rlog!(
                        WARNING,
                        "ralphus [runner] thinking-repetition stall detected ({} consecutive low-diversity samples over {}ms), killing squad={} cell={} last_line={:?}",
                        sample.consecutive_samples,
                        sample.span_ms,
                        spec.squad_id,
                        spec.cell_id,
                        sample.last_line,
                    );
                    note("direct run killed: thinking-repetition stall detected");
                    decided = Some(RunnerResult::thinking_stalled(
                        current_usage,
                        resumable_agent_session_id.clone(),
                        sample.last_line,
                    ));
                }
            }
            if decided.is_none() && !exited {
                std::thread::sleep(DIRECT_POLL_INTERVAL);
            }
        }

        let _ = child.wait();
        join_pumps(pumps);
        drain_transcript_to_current_end(
            &mut tailer,
            &target(),
            &mut resumable_agent_session_id,
            &mut current_usage,
        );

        let tail = transcript_tail(&transcript_path, SNAPSHOT_TAIL_LINES).unwrap_or_default();
        let mut result =
            decided.unwrap_or_else(|| Self::read_tmux_result(result_path, Some(tail.as_str())));
        let _ = std::fs::remove_file(spec_path);
        let _ = std::fs::remove_file(result_path);

        // The same two durable records the tmux path leaves behind: the
        // single-slot snapshot an ended Live View box shows, and this
        // attempt's terminal log.
        crate::tmux::write_pane_snapshot(session_name, &tail);
        let terminal_log_max_lines =
            crate::config::load_terminal_log_config().max_lines_per_attempt();
        write_terminal_log_preferring_raw_transcript(
            session_name,
            attempt,
            &tail,
            terminal_log_max_lines,
        );
        self.emit_terminal_log_note(spec, session_name, attempt);
        backfill_live_usage(&mut result, current_usage);
        result
    }

    /// A Cartographer note for a direct run's start or end.
    fn emit_direct_note(&self, spec: &RunnerSpec, message: &str, session_name: &str) {
        let Some(store) = &self.cartographer else {
            return;
        };
        let guard = store.lock();
        crate::cartographer::Note::new("runner")
            .squad(&spec.squad_id)
            .cell(&spec.cell_id)
            .task(&spec.task)
            .scope("direct")
            .emit(
                &guard,
                format!("{message} ({session_name})"),
                serde_json::json!({"session_name": session_name}),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_sink_stops_at_the_cap_and_marks_once() {
        let dir = std::env::temp_dir().join(format!("ralphus-direct-sink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.raw");
        let _ = std::fs::remove_file(&path);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut sink = CappedSink {
            file,
            written: 0,
            cap: 10,
            marked: false,
        };
        sink.write(b"0123456");
        sink.write(b"789abcdef");
        sink.write(b"more");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("0123456789"));
        assert!(!text.contains("abc"));
        assert_eq!(text.matches("truncated at 10 bytes").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcript_tail_returns_the_trailing_lines_without_ansi() {
        let dir = std::env::temp_dir().join(format!("ralphus-direct-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.raw");
        std::fs::write(&path, "one\r\n\u{1b}[31mtwo\u{1b}[0m\r\nthree\r\n").unwrap();
        assert_eq!(transcript_tail(&path, 2).unwrap(), "two\nthree");
        assert!(transcript_tail(&dir.join("missing.raw"), 2).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_guard_tracks_a_running_cell() {
        let name = "ralphus_direct_guard_test";
        assert!(!is_active(name));
        {
            let _guard = ActiveGuard::new(name);
            assert!(is_active(name));
        }
        assert!(!is_active(name));
    }
}
