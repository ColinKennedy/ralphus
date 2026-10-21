//! Durable asynchronous jobs: `exec` starts one, `status`/`stream` read it,
//! `cancel` stops it and `job-cleanup` forgets it.
//!
//! A job is a directory under `<root>/jobs/<hash(squad)>/<hash(cell)>/<handle>`
//! (the SSH provider's layout and handle format) holding:
//!
//! | file | written by | content |
//! |---|---|---|
//! | `spec.json` | `exec` | the cell spec, without its `execution_environment` |
//! | `runner.json` | `exec` | the argv prefix that reaches this runner |
//! | `created_at` | `exec` | milliseconds since the epoch |
//! | `state` | both | `starting`, `running`, `done`, `failed`, `cancelled` or `lost` |
//! | `heartbeat` | supervisor | milliseconds since the epoch, every [`HEARTBEAT`] |
//! | `output` | supervisor | the runner's stdout and stderr |
//! | `result` | supervisor | the runner's `CellResult` JSON |
//! | `cancel_requested` | `cancel` | who asked, and when |
//!
//! The job runs under a *supervisor* -- `<runner> machine supervise --job
//! <dir>` started detached ([`super::spawn`]) -- which runs `<runner> send`,
//! keeps the heartbeat fresh, watches for `cancel_requested`, and records the
//! result. A job whose heartbeat goes stale was lost (the machine rebooted,
//! the supervisor was killed). The job's environment -- secrets included --
//! is handed to the supervisor at process creation and inherited by the
//! runner; it is never written to the job directory.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::paths::Root;
use super::{Context, Events};

/// How often the supervisor refreshes its heartbeat.
pub const HEARTBEAT: Duration = Duration::from_secs(2);
/// A heartbeat older than this means the supervisor is gone.
const HEARTBEAT_STALE_MS: u128 = 20_000;
/// How long a job may sit in `starting` before its supervisor's first beat.
const STARTING_GRACE_MS: u128 = 30_000;
/// How long `cancel` waits for the supervisor to stop the job.
const CANCEL_WAIT: Duration = Duration::from_secs(20);
/// The most output one `stream` call returns.
const MAX_CHUNK: u64 = 65_536;
const CANCEL_REQUESTER: &str = "ralphus-daemon";

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn short_hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..16].to_string()
}

fn new_handle(squad: &str, cell: &str) -> Result<String, String> {
    let mut random = [0_u8; 16];
    getrandom_fill(&mut random)?;
    let entropy: String = random.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "v1-{}-{}-{entropy}",
        short_hash(squad),
        short_hash(cell)
    ))
}

fn getrandom_fill(buffer: &mut [u8]) -> Result<(), String> {
    getrandom::getrandom(buffer).map_err(|e| format!("could not generate a job handle: {e}"))
}

fn validate_handle(handle: &str) -> Result<(), String> {
    let parts: Vec<&str> = handle.split('-').collect();
    let valid = parts.len() == 4
        && parts[0] == "v1"
        && [16, 16, 32]
            .into_iter()
            .zip(&parts[1..])
            .all(|(len, part)| {
                part.len() == len
                    && part
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            });
    if valid {
        Ok(())
    } else {
        Err("invalid job handle".to_string())
    }
}

fn job_dir(root: &Root, handle: &str) -> Result<PathBuf, String> {
    validate_handle(handle)?;
    let parts: Vec<&str> = handle.split('-').collect();
    Ok(PathBuf::from(
        root.join(&format!("jobs/{}/{}/{handle}", parts[1], parts[2])),
    ))
}

fn handle_of(request: &Value) -> Result<&str, String> {
    super::required_str(request, "handle", "request")
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn write_text(path: &Path, text: &str) -> Result<(), String> {
    super::files::write_atomically(path, format!("{text}\n").as_bytes(), false)
}

fn read_ms(path: &Path) -> Option<u128> {
    read_text(path).and_then(|s| s.parse().ok())
}

/// Whether the job in `dir` (in `starting` or `running`) still has a live
/// supervisor.
fn alive(dir: &Path) -> bool {
    let now = now_ms();
    if let Some(beat) = read_ms(&dir.join("heartbeat")) {
        return now.saturating_sub(beat) <= HEARTBEAT_STALE_MS;
    }
    read_text(&dir.join("state")).as_deref() == Some("starting")
        && read_ms(&dir.join("created_at"))
            .is_some_and(|created| now.saturating_sub(created) <= STARTING_GRACE_MS)
}

fn mark_lost(dir: &Path) {
    let _ = write_text(&dir.join("state"), "lost");
}

/// `exec`: start the cell spec in `request` as a job and return its handle,
/// or the handle of the same squad/cell's earlier job while that one is
/// still alive.
///
/// # Errors
/// A malformed spec, a `cwd` outside the root, or a job that could not be
/// set up or started.
pub fn exec(
    request: &Value,
    root: &Root,
    context: &Context,
    events: &mut Events,
) -> Result<Value, String> {
    let Value::Object(mut spec) = request.clone() else {
        return Err("the cell spec must be a JSON object".to_string());
    };
    let environment = super::env_map(
        spec.remove("execution_environment").as_ref(),
        "execution_environment",
    )?;
    let spec = Value::Object(spec);
    let squad = super::required_str(&spec, "squad_id", "the cell spec")?;
    let cell = super::required_str(&spec, "cell_id", "the cell spec")?;
    let cwd = super::required_str(&spec, "cwd", "the cell spec")?;
    root.contain(cwd)?;
    if context.runner.is_empty() {
        return Err("this machine has no runner to start jobs with".to_string());
    }

    let dispatch_key = short_hash(&format!("{squad}\0{cell}"));
    let dispatch = PathBuf::from(root.join(&format!("jobs/dispatches/{dispatch_key}")));
    let _lock = super::lock::lock(
        Path::new(&root.join(&format!("jobs/.dispatch-locks/{dispatch_key}.lock"))),
        Duration::from_secs(60),
    )?;
    if let Some(previous) = read_text(&dispatch) {
        if let Some(handle) = previous
            .lines()
            .next()
            .filter(|h| validate_handle(h).is_ok())
        {
            let dir = job_dir(root, handle)?;
            let state = read_text(&dir.join("state")).unwrap_or_default();
            if matches!(state.as_str(), "starting" | "running") {
                if alive(&dir) {
                    events.emit(
                        "reattached to existing remote job",
                        "info",
                        json!({"handle": handle}),
                    );
                    return Ok(json!({"handle": handle}));
                }
                mark_lost(&dir);
            }
        }
    }
    let handle = new_handle(squad, cell)?;
    let dir = job_dir(root, &handle)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let protected = serde_json::to_vec(&spec).map_err(|e| e.to_string())?;
    super::files::write_atomically(&dir.join("spec.json"), &protected, false)?;
    let runner = serde_json::to_vec(&context.runner).map_err(|e| e.to_string())?;
    super::files::write_atomically(&dir.join("runner.json"), &runner, false)?;
    write_text(&dir.join("created_at"), &now_ms().to_string())?;
    write_text(&dir.join("state"), "starting")?;
    write_text(&dispatch, &handle)?;
    drop(_lock);

    let mut supervisor = Command::new(&context.runner[0]);
    supervisor
        .args(&context.runner[1..])
        .args(["machine", "supervise", "--job"])
        .arg(&dir)
        .envs(&environment);
    match super::spawn::spawn_detached(supervisor) {
        Ok(()) => {
            events.emit(
                "remote job started",
                "info",
                json!({"handle": handle, "job_dir": dir.to_string_lossy()}),
            );
            Ok(json!({"handle": handle}))
        }
        Err(super::spawn::DetachError::BreakawayRefused) => {
            // This process's job object (an SSH session's) would end a
            // detached supervisor with the session, so run the job here and
            // answer with its result instead of a handle.
            events.emit(
                "running remote job in the foreground",
                "warning",
                json!({
                    "handle": handle,
                    "reason": "the session's job object refused to let a detached process leave it",
                }),
            );
            supervise_with_env(&dir, &environment);
            let result = read_result(&dir)?;
            Ok(json!({"result": result}))
        }
        Err(super::spawn::DetachError::Other(error)) => {
            mark_lost(&dir);
            Err(format!("could not start the job supervisor: {error}"))
        }
    }
}

fn read_result(dir: &Path) -> Result<Value, String> {
    let raw = read_text(&dir.join("result")).unwrap_or_default();
    serde_json::from_str(&raw).map_err(|e| format!("job result is corrupt: {e}"))
}

/// `status`: the job's state, and its result once it has finished.
///
/// # Errors
/// A malformed handle, or a job that is missing, lost or corrupt.
pub fn status(request: &Value, root: &Root) -> Result<Value, String> {
    let handle = handle_of(request)?;
    let dir = job_dir(root, handle)?;
    if !dir.is_dir() {
        return Err(format!(
            "remote job {handle} is missing; durable job state cannot be reconciled safely"
        ));
    }
    let state = read_text(&dir.join("state")).unwrap_or_else(|| "corrupt".to_string());
    match state.as_str() {
        "starting" | "running" => {
            if alive(&dir) {
                Ok(json!({"state": state, "result": null}))
            } else {
                mark_lost(&dir);
                Err(format!(
                    "remote job {handle} is lost; durable job state cannot be reconciled safely"
                ))
            }
        }
        "done" | "failed" | "cancelled" => {
            let result = read_result(&dir)
                .map_err(|e| format!("remote job {handle} has terminal state {state:?} but {e}"))?;
            let state = if state == "done" { "done" } else { "failed" };
            Ok(json!({"state": state, "result": result}))
        }
        other => Err(format!(
            "remote job {handle} is {other}; durable job state cannot be reconciled safely"
        )),
    }
}

/// `stream`: output from byte `since`, whole lines only (unless one line
/// fills the chunk), and the cursor for the next call. Event lines in it
/// are also returned as events.
///
/// # Errors
/// A malformed handle or cursor, or unreadable output.
pub fn stream(request: &Value, root: &Root, events: &mut Events) -> Result<Value, String> {
    let handle = handle_of(request)?;
    let since = request.get("since").and_then(Value::as_i64).unwrap_or(0);
    let since =
        u64::try_from(since).map_err(|_| "stream cursor must be non-negative".to_string())?;
    let dir = job_dir(root, handle)?;
    let mut bytes = Vec::new();
    if let Ok(mut file) = std::fs::File::open(dir.join("output")) {
        file.seek(std::io::SeekFrom::Start(since))
            .map_err(|e| format!("could not read job output: {e}"))?;
        file.take(MAX_CHUNK)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("could not read job output: {e}"))?;
    }
    if let Some(end) = bytes.iter().rposition(|b| *b == b'\n') {
        bytes.truncate(end + 1);
    } else if (bytes.len() as u64) < MAX_CHUNK {
        bytes.clear();
    }
    let next = since + bytes.len() as u64;
    let output = String::from_utf8_lossy(&bytes).into_owned();
    for line in output.lines() {
        events.forward(line);
    }
    Ok(json!({"output": output, "next": next}))
}

/// `cancel`: ask the supervisor to stop the job and wait until it has. A job
/// already finished, or unknown, needs nothing.
///
/// # Errors
/// A malformed handle, or a job whose termination could not be confirmed.
pub fn cancel(request: &Value, root: &Root, events: &mut Events) -> Result<(), String> {
    let handle = handle_of(request)?;
    let dir = job_dir(root, handle)?;
    if !dir.is_dir() {
        return Ok(());
    }
    let terminal = |dir: &Path| {
        matches!(
            read_text(&dir.join("state")).as_deref(),
            Some("done" | "failed" | "cancelled")
        )
    };
    if terminal(&dir) {
        return Ok(());
    }
    write_text(
        &dir.join("cancel_requested"),
        &format!("{CANCEL_REQUESTER} {}", now_ms()),
    )?;
    let started = std::time::Instant::now();
    while started.elapsed() < CANCEL_WAIT {
        if terminal(&dir) {
            events.emit("cancelled remote job", "info", json!({"handle": handle}));
            return Ok(());
        }
        if !alive(&dir) {
            // Nothing is running it any more: record the cancellation here.
            finish(&dir, &cancelled_result(), "cancelled")?;
            events.emit("cancelled remote job", "info", json!({"handle": handle}));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = write_text(&dir.join("cancel_outcome"), "termination-unconfirmed");
    Err(format!(
        "remote job {handle}: termination could not be confirmed within {}s",
        CANCEL_WAIT.as_secs()
    ))
}

/// `job-cleanup`: remove a finished job's directory (never its worktree).
///
/// # Errors
/// A malformed handle, a job still running, or one lost or corrupt (whose
/// files are the only diagnostics left).
pub fn cleanup(request: &Value, root: &Root) -> Result<(), String> {
    let handle = handle_of(request)?;
    let dir = job_dir(root, handle)?;
    if !dir.is_dir() {
        return Ok(());
    }
    match read_text(&dir.join("state")).as_deref() {
        Some("starting" | "running") => {
            Err("refusing to clean up a running job; cancel it first".to_string())
        }
        Some("done" | "failed" | "cancelled") => super::files::remove_tree(&dir),
        Some("lost") | None => {
            Err("refusing to discard job diagnostics after infrastructure failure".to_string())
        }
        Some(_) => Err("refusing to clean up job with unknown state".to_string()),
    }
}

fn cancelled_result() -> Value {
    json!({"status": "failed", "summary": "cancelled", "error": "cancelled"})
}

/// Record a final result and state.
fn finish(dir: &Path, result: &Value, state: &str) -> Result<(), String> {
    write_text(&dir.join("result"), &result.to_string())?;
    write_text(&dir.join("state"), state)
}

/// Run the job in `dir` to completion: the body of `machine supervise`. Its
/// environment is this process's own, handed over at process creation.
pub fn supervise(dir: &Path) {
    supervise_with_env(dir, &BTreeMap::new());
}

fn supervise_with_env(dir: &Path, environment: &BTreeMap<String, String>) {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let heartbeat = {
        let path = dir.join("heartbeat");
        std::thread::spawn(move || {
            loop {
                let _ = write_text(&path, &now_ms().to_string());
                if !matches!(
                    stopped.recv_timeout(HEARTBEAT),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
            }
        })
    };
    let outcome = run_job(dir, environment);
    let (result, state) = match outcome {
        Ok(Outcome::Cancelled) => (cancelled_result(), "cancelled"),
        Ok(Outcome::Exited(code)) => match read_text(&dir.join("result.tmp"))
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        {
            Some(result) => {
                let failed = result.get("status").and_then(Value::as_str) == Some("failed");
                (result, if failed { "failed" } else { "done" })
            }
            None => (
                json!({
                    "status": "failed",
                    "summary": "remote runner produced no result",
                    "error": format!("remote runner exited {} without a result", code.map_or_else(|| "by a signal".to_string(), |c| c.to_string())),
                }),
                "failed",
            ),
        },
        Err(error) => (
            json!({
                "status": "failed",
                "summary": "remote runner could not be started",
                "error": error,
            }),
            "failed",
        ),
    };
    let _ = std::fs::remove_file(dir.join("result.tmp"));
    let _ = finish(dir, &result, state);
    if state == "cancelled" {
        let _ = write_text(&dir.join("cancel_outcome"), "terminated");
    }
    drop(stop);
    let _ = heartbeat.join();
}

enum Outcome {
    Exited(Option<i32>),
    Cancelled,
}

fn run_job(dir: &Path, environment: &BTreeMap<String, String>) -> Result<Outcome, String> {
    let runner: Vec<String> = std::fs::read(dir.join("runner.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .filter(|r: &Vec<String>| !r.is_empty())
        .ok_or_else(|| "the job has no runner.json".to_string())?;
    let spec: Value = std::fs::read(dir.join("spec.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .ok_or_else(|| "the job's spec.json is missing or corrupt".to_string())?;
    let cwd = spec
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| "the job's spec has no cwd".to_string())?
        .to_string();
    write_text(&dir.join("state"), "running")?;
    let output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("output"))
        .map_err(|e| format!("could not open the job output: {e}"))?;
    let errors = output
        .try_clone()
        .map_err(|e| format!("could not open the job output: {e}"))?;
    let spec_file = std::fs::File::open(dir.join("spec.json"))
        .map_err(|e| format!("could not open the job spec: {e}"))?;
    let mut command = Command::new(&runner[0]);
    command
        .args(&runner[1..])
        .arg("send")
        .arg("--result-file")
        .arg(dir.join("result.tmp"))
        .current_dir(&cwd)
        .envs(environment)
        .stdin(spec_file)
        .stdout(output)
        .stderr(errors);
    super::spawn::prepare_tree(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", runner[0]))?;
    let mut tree = super::spawn::Tree::confine(&child);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Outcome::Exited(status.code())),
            Ok(None) => {}
            Err(e) => return Err(format!("could not wait for the runner: {e}")),
        }
        if dir.join("cancel_requested").exists() {
            tree.kill(&mut child);
            let _ = child.wait();
            return Ok(Outcome::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_unguessable_and_name_their_directory() {
        let a = new_handle("s", "c").unwrap();
        let b = new_handle("s", "c").unwrap();
        assert_ne!(a, b);
        assert!(validate_handle(&a).is_ok());
        assert!(validate_handle("../../etc").is_err());
        let root = Root::new(if cfg!(windows) { "C:/r" } else { "/r" }).unwrap();
        let dir = job_dir(&root, &a).unwrap();
        assert!(
            dir.to_string_lossy().contains("/jobs/"),
            "{}",
            dir.display()
        );
    }

    #[test]
    fn a_stale_heartbeat_is_a_lost_job_and_a_fresh_one_is_running() {
        let dir = ralphus_test_support::TestDir::new("machine-job-liveness");
        write_text(&dir.join("state"), "running").unwrap();
        write_text(&dir.join("heartbeat"), &now_ms().to_string()).unwrap();
        assert!(alive(&dir));
        write_text(&dir.join("heartbeat"), "1").unwrap();
        assert!(!alive(&dir));
        std::fs::remove_file(dir.join("heartbeat")).unwrap();
        write_text(&dir.join("state"), "starting").unwrap();
        write_text(&dir.join("created_at"), &now_ms().to_string()).unwrap();
        assert!(alive(&dir), "a job still starting gets a grace period");
    }

    #[test]
    fn stream_returns_whole_lines_and_their_events() {
        let dir = ralphus_test_support::TestDir::new("machine-job-stream");
        let root = Root::new(&dir.to_string_lossy()).unwrap();
        let handle = new_handle("s", "c").unwrap();
        let job = job_dir(&root, &handle).unwrap();
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(
            job.join("output"),
            "line one\nRALPHUS_EVENT: {\"source\":\"llm-invoke\"}\npartial",
        )
        .unwrap();
        let mut events = Events::default();
        let reply = stream(&json!({"handle": handle, "since": 0}), &root, &mut events).unwrap();
        let output = reply["output"].as_str().unwrap();
        assert!(output.ends_with("}\n"), "{output}");
        assert_eq!(reply["next"], output.len());
        assert_eq!(events.0, [r#"{"source":"llm-invoke"}"#]);
        let rest = stream(
            &json!({"handle": handle, "since": reply["next"]}),
            &root,
            &mut events,
        )
        .unwrap();
        assert_eq!(rest["output"], "", "a partial line waits");
    }

    #[test]
    fn cleanup_refuses_running_and_lost_jobs() {
        let dir = ralphus_test_support::TestDir::new("machine-job-cleanup");
        let root = Root::new(&dir.to_string_lossy()).unwrap();
        let handle = new_handle("s", "c").unwrap();
        let job = job_dir(&root, &handle).unwrap();
        std::fs::create_dir_all(&job).unwrap();
        for (state, ok) in [("running", false), ("lost", false), ("done", true)] {
            write_text(&job.join("state"), state).unwrap();
            assert_eq!(
                cleanup(&json!({"handle": handle}), &root).is_ok(),
                ok,
                "{state}"
            );
        }
        assert!(!job.exists());
    }
}
