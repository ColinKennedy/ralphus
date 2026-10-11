//! Liveness registry for the daemon's long-lived background threads.
//!
//! A panic in one of these threads does not take the daemon down: the thread
//! just ends, and whatever it drove stops silently. The scheduler thread is
//! the clearest case -- it runs the squad `tick`, the review base-shift
//! rebuild, the auto-submit sweep and the base-branch fetch poll, so when it
//! dies reviews stop rebasing and PRs stop being submitted while the HTTP API
//! keeps answering as if nothing were wrong.
//!
//! [`spawn_supervised`] runs a thread body under `catch_unwind` and records
//! how it ended. [`beat`] lets a looping body prove it is still turning over.
//! [`snapshot`] reports each thread's state for the health surfaces, and
//! [`restart`] re-spawns a thread that has ended.
//!
//! A panicked or returned thread is **not** restarted automatically: the
//! panic usually names a real defect, and a restart that hides it would also
//! hide the notification. The panic is instead pushed to the mailbox with a
//! restart command as its remediation.
//!
//! Only a thread that has ended can be restarted. A thread that is merely
//! silent (`stalled`) is still alive and cannot be killed from here, so a
//! second copy is never spawned beside it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::store_lock::StoreHandle;

/// How long the panic path waits for the store lock before giving up on the
/// mailbox message. The panic that killed the thread may itself have come from
/// a long lock hold, so the notifier must never queue behind it indefinitely.
const NOTIFY_LOCK_WAIT: Duration = Duration::from_secs(5);

/// Static description of one supervised thread.
#[derive(Debug, Clone, Copy)]
pub struct ThreadSpec {
    /// Stable id used by the restart command and the health check detail.
    pub name: &'static str,
    /// One line on what stops working when this thread is down.
    pub description: &'static str,
    /// How long the body may go without calling [`beat`] before it is called
    /// `stalled`. `None` for a body that sleeps longer than any sensible
    /// threshold and so is judged by whether it has ended, not by silence.
    pub max_silence: Option<Duration>,
}

/// A re-runnable thread body: calling it again is how a restart works.
pub type ThreadBody = Arc<dyn Fn() + Send + Sync + 'static>;

/// What the registry knows a thread to be doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    /// Running and (when it has a `max_silence`) beating on time.
    Running,
    /// Still alive but past its `max_silence` without a beat.
    Stalled,
    /// Ended by a panic.
    Panicked,
    /// Ended by returning, which a body that loops forever never does.
    Exited,
}

/// One thread's state as reported to the health surfaces.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ThreadStatus {
    pub name: &'static str,
    pub description: &'static str,
    pub state: ThreadState,
    /// Milliseconds since the last [`beat`] (or the thread's start, if it has
    /// not beaten yet).
    pub silent_for_ms: u64,
    pub max_silence_ms: Option<u64>,
    pub panic_message: Option<String>,
    pub panicked_at_ms: Option<u64>,
    /// How many times [`restart`] has re-spawned this thread.
    pub restarts: u64,
    /// Whether [`restart`] would be accepted right now.
    pub restartable: bool,
}

impl ThreadStatus {
    /// The health-sweep status string for this thread.
    #[must_use]
    pub fn health(&self) -> &'static str {
        match self.state {
            ThreadState::Running => "pass",
            ThreadState::Stalled => "warn",
            ThreadState::Panicked | ThreadState::Exited => "fail",
        }
    }

    /// A single line naming the thread and what happened to it.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.state {
            ThreadState::Running => format!("{}: running", self.name),
            ThreadState::Stalled => format!(
                "{}: no heartbeat for {}s (limit {}s)",
                self.name,
                self.silent_for_ms / 1000,
                self.max_silence_ms.unwrap_or(0) / 1000
            ),
            ThreadState::Panicked => format!(
                "{}: panicked ({})",
                self.name,
                self.panic_message.as_deref().unwrap_or("no message")
            ),
            ThreadState::Exited => format!("{}: exited unexpectedly", self.name),
        }
    }
}

struct Entry {
    spec: ThreadSpec,
    body: ThreadBody,
    store: StoreHandle,
    /// Bumped on every (re)spawn so a late exit report from an older
    /// incarnation cannot overwrite the state of the one that replaced it.
    generation: u64,
    ended: Option<Ended>,
    last_beat: Instant,
    restarts: u64,
}

struct Ended {
    panic_message: Option<String>,
    at_ms: u64,
}

static REGISTRY: LazyLock<Mutex<BTreeMap<&'static str, Entry>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

static GENERATION: AtomicU64 = AtomicU64::new(0);

fn registry() -> std::sync::MutexGuard<'static, BTreeMap<&'static str, Entry>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Registers `spec` and runs `body` on a new thread, recording how it ends.
///
/// `body` must be safe to call again from the top: [`restart`] re-invokes it
/// as-is, so anything that must run once per process belongs outside it.
pub fn spawn_supervised(spec: ThreadSpec, store: StoreHandle, body: ThreadBody) {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    registry().insert(
        spec.name,
        Entry {
            spec,
            body: Arc::clone(&body),
            store,
            generation,
            ended: None,
            last_beat: Instant::now(),
            restarts: 0,
        },
    );
    launch(spec.name, generation, body);
}

fn launch(name: &'static str, generation: u64, body: ThreadBody) {
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body()));
        let message = outcome.as_ref().err().map(|p| panic_message(p.as_ref()));
        record_end(name, generation, message);
    });
}

fn record_end(name: &'static str, generation: u64, panic_message: Option<String>) {
    let (spec, store) = {
        let mut map = registry();
        let Some(entry) = map.get_mut(name) else {
            return;
        };
        if entry.generation != generation {
            return;
        }
        entry.ended = Some(Ended {
            panic_message: panic_message.clone(),
            at_ms: now_ms(),
        });
        (entry.spec, Arc::clone(&entry.store))
    };
    let how = panic_message
        .as_ref()
        .map_or_else(|| "returned".to_string(), |m| format!("panicked: {m}"));
    crate::rlog!(
        ERROR,
        "ralphus [thread_health] daemon thread '{}' {how}",
        spec.name
    );
    notify(spec, &store, &how);
}

/// Cartographer row and mailbox message for a thread that has ended. Uses a
/// bounded lock wait because the store lock may be exactly what the dead
/// thread was holding the daemon up on.
fn notify(spec: ThreadSpec, store: &StoreHandle, how: &str) {
    let Some(guard) = store.try_lock_for(NOTIFY_LOCK_WAIT) else {
        crate::rlog!(
            ERROR,
            "ralphus [thread_health] store lock unavailable; no mailbox notice for '{}'",
            spec.name
        );
        return;
    };
    crate::cartographer::Note::new("thread_health")
        .level(crate::logging::LogLevel::ERROR)
        .scope("daemon")
        .emit(
            &guard,
            format!("daemon thread '{}' {how}", spec.name),
            serde_json::json!({"thread": spec.name, "outcome": how}),
        );
    let text = format!(
        "The daemon's '{}' thread {how}. {}",
        spec.name, spec.description
    );
    let entity_uri = format!("daemon-thread:{}", spec.name);
    if let Err(e) = guard.enqueue_error_mailbox_message_deduped(
        crate::mailbox::MailboxPriority::Urgent,
        &text,
        &crate::mailbox::Remediation::SuggestedCommand {
            command: format!("ralphus check restart-thread {}", spec.name),
            purpose: format!(
                "restart the '{}' thread without restarting the daemon",
                spec.name
            ),
        },
        None,
        None,
        None,
        Some(&entity_uri),
        Some("daemon-thread"),
    ) {
        crate::rlog!(
            ERROR,
            "ralphus [thread_health] could not enqueue mailbox notice for '{}': {e}",
            spec.name
        );
    }
}

/// Marks `name` as having just made progress. A no-op for an unregistered
/// name, so a body can beat unconditionally.
pub fn beat(name: &str) {
    if let Some(entry) = registry().get_mut(name) {
        entry.last_beat = Instant::now();
    }
}

fn status_of(entry: &Entry) -> ThreadStatus {
    let silent = entry.last_beat.elapsed();
    let state = match (&entry.ended, entry.spec.max_silence) {
        (Some(ended), _) if ended.panic_message.is_some() => ThreadState::Panicked,
        (Some(_), _) => ThreadState::Exited,
        (None, Some(limit)) if silent > limit => ThreadState::Stalled,
        (None, _) => ThreadState::Running,
    };
    ThreadStatus {
        name: entry.spec.name,
        description: entry.spec.description,
        state,
        silent_for_ms: u64::try_from(silent.as_millis()).unwrap_or(u64::MAX),
        max_silence_ms: entry
            .spec
            .max_silence
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        panic_message: entry.ended.as_ref().and_then(|e| e.panic_message.clone()),
        panicked_at_ms: entry.ended.as_ref().map(|e| e.at_ms),
        restarts: entry.restarts,
        restartable: entry.ended.is_some(),
    }
}

/// Every registered thread's current state, ordered by name.
#[must_use]
pub fn snapshot() -> Vec<ThreadStatus> {
    registry().values().map(status_of).collect()
}

/// The worst status among `threads` as a health string, with one summary line
/// per thread that is not running. `("skip", ..)` when nothing is registered
/// (a unit-test daemon that never called [`spawn_supervised`]).
#[must_use]
pub fn rollup(threads: &[ThreadStatus]) -> (&'static str, String) {
    if threads.is_empty() {
        return ("skip", "no supervised daemon threads are registered".into());
    }
    let worst = threads
        .iter()
        .map(ThreadStatus::health)
        .fold("pass", |acc, s| match (acc, s) {
            ("fail", _) | (_, "fail") => "fail",
            ("warn", _) | (_, "warn") => "warn",
            _ => "pass",
        });
    let bad: Vec<String> = threads
        .iter()
        .filter(|t| t.state != ThreadState::Running)
        .map(ThreadStatus::summary)
        .collect();
    let detail = if bad.is_empty() {
        format!("{} supervised threads running", threads.len())
    } else {
        bad.join("; ")
    };
    (worst, detail)
}

/// Why a restart request was not carried out.
#[derive(Debug, PartialEq, Eq)]
pub enum RestartError {
    Unknown,
    /// The thread is alive; a second copy beside it would be worse than none.
    StillAlive(ThreadState),
}

impl std::fmt::Display for RestartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("no such supervised thread"),
            Self::StillAlive(state) => write!(
                f,
                "thread has not ended (state: {state:?}); a live thread cannot be killed, \
                 so only a panicked or exited thread can be restarted"
            ),
        }
    }
}

/// Re-spawns `name` if it has ended. Returns its refreshed status.
///
/// # Errors
/// [`RestartError::Unknown`] for an unregistered name;
/// [`RestartError::StillAlive`] while the thread has not ended.
pub fn restart(name: &str) -> Result<ThreadStatus, RestartError> {
    let (key, generation, body) = {
        let mut map = registry();
        let entry = map.get_mut(name).ok_or(RestartError::Unknown)?;
        if entry.ended.is_none() {
            return Err(RestartError::StillAlive(status_of(entry).state));
        }
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        entry.generation = generation;
        entry.ended = None;
        entry.last_beat = Instant::now();
        entry.restarts += 1;
        (entry.spec.name, generation, Arc::clone(&entry.body))
    };
    crate::rlog!(
        WARNING,
        "ralphus [thread_health] restarting daemon thread '{key}'"
    );
    launch(key, generation, body);
    let map = registry();
    Ok(status_of(&map[key]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn store() -> StoreHandle {
        Arc::new(crate::store_lock::StoreMutex::new(
            crate::store::Store::open_in_memory().expect("open in-memory store"),
        ))
    }

    fn wait_for(name: &str, want: impl Fn(&ThreadStatus) -> bool) -> ThreadStatus {
        for _ in 0..200 {
            if let Some(s) = snapshot().into_iter().find(|s| s.name == name) {
                if want(&s) {
                    return s;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "thread {name} never reached the wanted state: {:?}",
            snapshot()
        );
    }

    fn spec(name: &'static str, max_silence: Option<Duration>) -> ThreadSpec {
        ThreadSpec {
            name,
            description: "test thread",
            max_silence,
        }
    }

    #[test]
    fn a_panicking_thread_is_reported_panicked_with_its_message() {
        spawn_supervised(
            spec("th-test-panics", None),
            store(),
            Arc::new(|| panic!("boom from test")),
        );
        let s = wait_for("th-test-panics", |s| s.state == ThreadState::Panicked);
        assert_eq!(s.health(), "fail");
        assert_eq!(s.panic_message.as_deref(), Some("boom from test"));
        assert!(s.restartable);
    }

    #[test]
    fn a_returning_thread_is_reported_exited() {
        spawn_supervised(spec("th-test-returns", None), store(), Arc::new(|| {}));
        let s = wait_for("th-test-returns", |s| s.state == ThreadState::Exited);
        assert_eq!(s.health(), "fail");
        assert!(s.panic_message.is_none());
    }

    #[test]
    fn a_live_thread_cannot_be_restarted() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        spawn_supervised(
            spec("th-test-live", None),
            store(),
            Arc::new(move || {
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }),
        );
        assert_eq!(
            restart("th-test-live").unwrap_err(),
            RestartError::StillAlive(ThreadState::Running)
        );
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn a_silent_thread_is_stalled_and_a_beat_clears_it() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        spawn_supervised(
            spec("th-test-stall", Some(Duration::from_millis(40))),
            store(),
            Arc::new(move || {
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }),
        );
        let s = wait_for("th-test-stall", |s| s.state == ThreadState::Stalled);
        assert_eq!(s.health(), "warn");
        assert!(
            !s.restartable,
            "a stalled thread is alive, so not restartable"
        );
        beat("th-test-stall");
        assert_eq!(
            snapshot()
                .into_iter()
                .find(|s| s.name == "th-test-stall")
                .unwrap()
                .state,
            ThreadState::Running
        );
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn restart_respawns_an_ended_thread_and_ignores_the_old_generations_exit() {
        let runs = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&runs);
        spawn_supervised(
            spec("th-test-restart", None),
            store(),
            Arc::new(move || {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("first run dies");
                }
                std::thread::sleep(Duration::from_secs(30));
            }),
        );
        wait_for("th-test-restart", |s| s.state == ThreadState::Panicked);
        let restarted = restart("th-test-restart").expect("restart accepted");
        assert_eq!(restarted.restarts, 1);
        let s = wait_for("th-test-restart", |s| s.state == ThreadState::Running);
        assert!(s.panic_message.is_none());
        for _ in 0..200 {
            if runs.load(Ordering::SeqCst) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the body ran a second time");
    }

    #[test]
    fn restart_of_an_unknown_name_is_rejected() {
        assert_eq!(restart("th-test-nope").unwrap_err(), RestartError::Unknown);
    }

    #[test]
    fn a_panic_enqueues_an_urgent_mailbox_message_with_the_restart_command() {
        let store = store();
        spawn_supervised(
            spec("th-test-mailbox", None),
            Arc::clone(&store),
            Arc::new(|| panic!("mailbox boom")),
        );
        wait_for("th-test-mailbox", |s| s.state == ThreadState::Panicked);
        for _ in 0..200 {
            let found = {
                let guard = store.lock();
                guard
                    .mailbox_messages_for_client("thread-health-test", false, None)
                    .unwrap_or_default()
                    .iter()
                    .any(|m| {
                        m.message.contains("th-test-mailbox")
                            && m.message
                                .contains("ralphus check restart-thread th-test-mailbox")
                    })
            };
            if found {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no mailbox message carrying the restart command was enqueued");
    }

    #[test]
    fn rollup_takes_the_worst_state_and_names_only_the_unhealthy() {
        let status = |name, state| ThreadStatus {
            name,
            description: "",
            state,
            silent_for_ms: 0,
            max_silence_ms: None,
            panic_message: None,
            panicked_at_ms: None,
            restarts: 0,
            restartable: false,
        };
        let (health, detail) = rollup(&[
            status("a", ThreadState::Running),
            status("b", ThreadState::Stalled),
            status("c", ThreadState::Exited),
        ]);
        assert_eq!(health, "fail");
        assert!(detail.contains("b:") && detail.contains("c:") && !detail.contains("a:"));
        assert_eq!(rollup(&[]).0, "skip");
        assert_eq!(rollup(&[status("a", ThreadState::Running)]).0, "pass");
    }
}
