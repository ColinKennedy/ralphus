//! RAL-596: daemon-wide health signals, exported as OpenTelemetry spans.
//!
//! Scope is the daemon itself -- a wedged store lock, a stalled or dead
//! background loop, a startup/fatal failure, a nearly
//! full disk, and a positive "daemon is up" heartbeat. Failures inside one
//! review, squad or cell stay in their own logs and Cartographer rows and are
//! deliberately never sent here.
//!
//! # Emission path
//!
//! [`emit`] only pushes onto a bounded channel (`try_send`, drop on full) read
//! by one emitter thread, which is the only thread that creates spans. The
//! exporter in [`crate::otel`] is a *simple* exporter: ending a span is a
//! blocking HTTP call. Doing that from `Drop for StoreGuard` (global lock
//! held) or from a panic hook would let a slow collector stall the daemon on
//! its own health signal, so no caller ever touches the exporter directly.
//!
//! Until [`start_emitter`] runs (OpenTelemetry disabled or not yet
//! initialised) [`emit`] is a single `OnceLock` load and returns.
//!
//! # Event names
//!
//! | Span | Meaning |
//! |---|---|
//! | `daemon.heartbeat` | daemon is up (every 60 s) |
//! | `daemon.startup` / `daemon.shutdown` | lifecycle |
//! | `daemon.startup_failed` | license, store open, port bind, token |
//! | `daemon.panic` | any thread panicked |
//! | `daemon.store_lock.slow_wait` / `.long_hold` | wait >= 2 s / hold >= 1 s |
//! | `daemon.store_lock.stalled` / `.recovered` | watchdog round result |
//! | `daemon.loop.stale` / `.recovered` / `.dead` | scheduler / health sweep |
//! //! | `daemon.resource.low_disk` | free space under 10 % / 5 % |
//! | `daemon.resource.threads_high` / `.threads_normal` | process thread count crosses 500 (Linux) |
//! | `daemon.sqlite.busy` / `.write_failed` | busy timeout / disk full, I/O, read-only (max one a minute each) |

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use opentelemetry::trace::{SpanKind, Status};
use opentelemetry::{Context, Value};

use crate::store_lock::StoreHandle;

/// Queued events beyond this are dropped: a dead collector must cost the
/// daemon nothing but missing spans.
const QUEUE_CAPACITY: usize = 256;

/// How often the daemon-up heartbeat span is emitted.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// Cadence of the slow probes (disk space).
pub const PROBE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Grace after startup before the first slow probe, so it does not compete
/// with startup recovery.
const FIRST_PROBE_DELAY: Duration = Duration::from_secs(60);

/// Free-space fraction (percent) below which a volume is reported as low.
pub const DISK_WARN_FREE_PERCENT: f64 = 10.0;
/// Free-space fraction (percent) below which a volume is reported as critical.
pub const DISK_CRITICAL_FREE_PERCENT: f64 = 5.0;

/// One health event, turned into a span by the emitter thread.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthEvent {
    /// Span name, e.g. `daemon.heartbeat`.
    pub name: &'static str,
    /// `Some(message)` marks the span with error status.
    pub error: Option<String>,
    /// Span attributes.
    pub attrs: Vec<(&'static str, Value)>,
}

impl HealthEvent {
    /// A healthy / informational event.
    #[must_use]
    pub fn ok(name: &'static str) -> Self {
        Self {
            name,
            error: None,
            attrs: Vec::new(),
        }
    }

    /// An event reporting a bad condition (error span status).
    #[must_use]
    pub fn error(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            name,
            error: Some(message.into()),
            attrs: Vec::new(),
        }
    }

    /// Attach an attribute.
    #[must_use]
    pub fn attr(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.attrs.push((key, value.into()));
        self
    }
}

struct Emitter {
    tx: SyncSender<HealthEvent>,
    /// Events queued or being exported, so [`flush`] can wait for them.
    pending: Arc<AtomicUsize>,
}

static EMITTER: OnceLock<Emitter> = OnceLock::new();

/// Start the emitter thread. Call once, after `otel::init` returned a
/// provider; never call when OpenTelemetry is disabled (emission then stays a
/// no-op). Returns `false` if already started.
pub fn start_emitter() -> bool {
    let (tx, rx) = sync_channel(QUEUE_CAPACITY);
    let pending = Arc::new(AtomicUsize::new(0));
    if EMITTER
        .set(Emitter {
            tx,
            pending: Arc::clone(&pending),
        })
        .is_err()
    {
        return false;
    }
    std::thread::Builder::new()
        .name("ralphus-health-emitter".to_string())
        .spawn(move || run_emitter(&rx, &pending))
        .is_ok()
}

fn run_emitter(rx: &Receiver<HealthEvent>, pending: &AtomicUsize) {
    while let Ok(event) = rx.recv() {
        deliver(&event);
        pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Turn one event into a finished span (the blocking export happens here).
fn deliver(event: &HealthEvent) {
    let span = crate::otel::start_span(event.name, &Context::new(), SpanKind::Internal);
    for (key, value) in &event.attrs {
        span.set_attribute(key, value.clone());
    }
    if let Some(message) = &event.error {
        span.set_status(Status::error(message.clone()));
    }
}

/// Queue `event` for export. Never blocks and never fails: a no-op when the
/// emitter was not started, and the event is dropped when the queue is full.
pub fn emit(event: HealthEvent) {
    if let Some(emitter) = EMITTER.get() {
        emitter.pending.fetch_add(1, Ordering::AcqRel);
        if try_send(&emitter.tx, event).is_err() {
            emitter.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Non-blocking send used by [`emit`]; `Err(())` when the event was dropped.
fn try_send(tx: &SyncSender<HealthEvent>, event: HealthEvent) -> Result<(), ()> {
    match tx.try_send(event) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(()),
    }
}

/// Wait up to `timeout` for queued events to be exported -- used right before
/// the process exits so a fatal-startup span is not lost.
pub fn flush(timeout: Duration) {
    let Some(emitter) = EMITTER.get() else {
        return;
    };
    let deadline = Instant::now() + timeout;
    while emitter.pending.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Install a process-wide panic hook that emits `daemon.panic` (thread name +
/// location) and then runs the previously installed hook.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let location = info
            .location()
            .map_or_else(|| "unknown".to_string(), ToString::to_string);
        emit(
            HealthEvent::error("daemon.panic", "a daemon thread panicked")
                .attr("thread", thread.name().unwrap_or("<unnamed>").to_string())
                .attr("location", location),
        );
        previous(info);
    }));
}

/// Report a fatal startup failure at `stage` and wait briefly for export.
pub fn startup_failed(stage: &'static str, error: &str) {
    emit(
        HealthEvent::error("daemon.startup_failed", format!("{stage}: {error}"))
            .attr("stage", stage)
            .attr("error", error.to_string()),
    );
    flush(Duration::from_secs(5));
}

// ── Disk space ──────────────────────────────────────────────────────────

/// How low a volume's free space is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskSeverity {
    Warning,
    Critical,
}

impl DiskSeverity {
    fn label(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// Classify a free-space percentage; `None` when there is enough room.
#[must_use]
pub fn classify_free_percent(free_percent: f64) -> Option<DiskSeverity> {
    if free_percent < DISK_CRITICAL_FREE_PERCENT {
        Some(DiskSeverity::Critical)
    } else if free_percent < DISK_WARN_FREE_PERCENT {
        Some(DiskSeverity::Warning)
    } else {
        None
    }
}

/// One mounted volume: `(mount point, available bytes, total bytes)`.
pub type Volume = (std::path::PathBuf, u64, u64);

/// The volume holding `path`: the entry whose mount point is the longest
/// prefix of it. Windows verbatim prefixes (`\\?\`) are stripped first.
#[must_use]
pub fn volume_for_path<'a>(volumes: &'a [Volume], path: &std::path::Path) -> Option<&'a Volume> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy();
    let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let target = std::path::Path::new(stripped);
    volumes
        .iter()
        .filter(|(mount, _, total)| *total > 0 && target.starts_with(mount))
        .max_by_key(|(mount, _, _)| mount.as_os_str().len())
}

fn list_volumes() -> Vec<Volume> {
    sysinfo::Disks::new_with_refreshed_list()
        .list()
        .iter()
        .map(|d| {
            (
                d.mount_point().to_path_buf(),
                d.available_space(),
                d.total_space(),
            )
        })
        .collect()
}

/// Check each of `labelled_paths` (`(label, path)`) against the thresholds,
/// warn-logging and emitting one `daemon.resource.low_disk` per distinct
/// low volume. Returns the number of volumes reported.
pub fn check_disk_space(labelled_paths: &[(&'static str, std::path::PathBuf)]) -> usize {
    let volumes = list_volumes();
    let mut reported: Vec<std::path::PathBuf> = Vec::new();
    for (label, path) in labelled_paths {
        let Some((mount, avail, total)) = volume_for_path(&volumes, path) else {
            continue;
        };
        if reported.contains(mount) {
            continue;
        }
        let free_percent = *avail as f64 * 100.0 / *total as f64;
        let Some(severity) = classify_free_percent(free_percent) else {
            continue;
        };
        reported.push(mount.clone());
        let message = format!(
            "{label} volume {} is {severity} on disk space: {free_percent:.1}% free ({} MiB of {} MiB)",
            mount.display(),
            avail / (1024 * 1024),
            total / (1024 * 1024),
            severity = severity.label(),
        );
        // ralphus[ignore-rlog-pair]: periodic probe with no single entity to attach a Cartographer row to; the OTel span is its structured record
        crate::rlog!(WARNING, "ralphus [resource] {message}");
        let event = if severity == DiskSeverity::Critical {
            HealthEvent::error("daemon.resource.low_disk", message)
        } else {
            HealthEvent::ok("daemon.resource.low_disk")
        };
        emit(
            event
                .attr("volume", mount.display().to_string())
                .attr("role", *label)
                .attr("severity", severity.label())
                .attr("free_percent", free_percent)
                .attr("free_bytes", i64::try_from(*avail).unwrap_or(i64::MAX)),
        );
    }
    reported.len()
}

// ── SQLite busy / write failures ────────────────────────────────────────

/// Minimum gap between two spans of the same SQLite kind, so a failing disk
/// that errors on every statement produces one span a minute, not thousands.
const SQLITE_EMIT_GAP_MS: i64 = 60_000;

/// What kind of daemon-level SQLite trouble an error is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteTrouble {
    /// `SQLITE_BUSY` / `SQLITE_LOCKED`: the busy timeout ran out.
    Busy,
    /// Disk full, read-only database, I/O failure or an unopenable file.
    WriteFailure,
}

impl SqliteTrouble {
    const fn event_name(self) -> &'static str {
        match self {
            Self::Busy => "daemon.sqlite.busy",
            Self::WriteFailure => "daemon.sqlite.write_failed",
        }
    }

    const fn slot(self) -> usize {
        match self {
            Self::Busy => 0,
            Self::WriteFailure => 1,
        }
    }
}

/// Classify a rusqlite error; `None` for ordinary query errors (constraint
/// violations, missing rows, ...), which are per-operation and not daemon health.
#[must_use]
pub fn classify_sqlite_error(e: &rusqlite::Error) -> Option<SqliteTrouble> {
    use rusqlite::ErrorCode as C;
    match e.sqlite_error_code()? {
        C::DatabaseBusy | C::DatabaseLocked => Some(SqliteTrouble::Busy),
        C::DiskFull | C::ReadOnly | C::SystemIoFailure | C::CannotOpen | C::DatabaseCorrupt => {
            Some(SqliteTrouble::WriteFailure)
        }
        _ => None,
    }
}

static SQLITE_COUNTS: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
static SQLITE_LAST_EMIT_MS: [std::sync::atomic::AtomicI64; 2] = [
    std::sync::atomic::AtomicI64::new(0),
    std::sync::atomic::AtomicI64::new(0),
];

/// Count `e` and, at most once a minute per kind, emit its health span.
/// Lock-free and non-blocking: called from the `rusqlite::Error` conversion,
/// which runs on any thread including ones holding the store lock.
pub fn report_sqlite_error(e: &rusqlite::Error) {
    let Some(kind) = classify_sqlite_error(e) else {
        return;
    };
    let total = SQLITE_COUNTS[kind.slot()].fetch_add(1, Ordering::Relaxed) + 1;
    if sqlite_emit_due(&SQLITE_LAST_EMIT_MS[kind.slot()], crate::store::now_ms()) {
        emit(
            HealthEvent::error(kind.event_name(), e.to_string())
                .attr("total", i64::try_from(total).unwrap_or(i64::MAX)),
        );
    }
}

/// Whether a span is due given the last emission stamp; claims the slot.
fn sqlite_emit_due(last: &std::sync::atomic::AtomicI64, now_ms: i64) -> bool {
    let prev = last.load(Ordering::Relaxed);
    now_ms.saturating_sub(prev) >= SQLITE_EMIT_GAP_MS
        && last
            .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

// ── Thread count ────────────────────────────────────────────────────────

/// OS threads in the daemon process above which the count is reported.
pub const THREAD_COUNT_WARN: usize = 500;

static THREADS_HIGH: AtomicBool = AtomicBool::new(false);

/// OS threads in this process. Only Linux exposes this without `unsafe`
/// (`/proc/self/task`); other platforms return `None` and are not monitored.
#[must_use]
pub fn process_thread_count() -> Option<usize> {
    std::fs::read_dir("/proc/self/task")
        .ok()
        .map(|d| d.filter_map(Result::ok).count())
}

/// Event for a thread-count observation when the high/normal state changed.
fn thread_count_transition(high: &AtomicBool, count: usize) -> Option<HealthEvent> {
    let now_high = count >= THREAD_COUNT_WARN;
    (high.swap(now_high, Ordering::AcqRel) != now_high).then(|| {
        let count = i64::try_from(count).unwrap_or(i64::MAX);
        if now_high {
            HealthEvent::error(
                "daemon.resource.threads_high",
                format!("{count} threads in the daemon process"),
            )
            .attr("threads", count)
        } else {
            HealthEvent::ok("daemon.resource.threads_normal").attr("threads", count)
        }
    })
}

/// Sample the thread count and emit on a state change; returns the count.
pub fn check_thread_count() -> Option<usize> {
    let count = process_thread_count()?;
    if let Some(event) = thread_count_transition(&THREADS_HIGH, count) {
        // ralphus[ignore-rlog-pair]: process-wide probe with no entity to attach a Cartographer row to; the OTel span is its structured record
        crate::rlog!(WARNING, "ralphus [resource] daemon thread count: {count}");
        emit(event);
    }
    Some(count)
}

/// Spawn the slow-probe thread: disk space every
/// [`PROBE_INTERVAL`].
pub fn spawn_periodic_probes(store: StoreHandle) {
    let spawned = std::thread::Builder::new()
        .name("ralphus-health-probes".to_string())
        .spawn(move || {
            std::thread::sleep(FIRST_PROBE_DELAY);
            loop {
                let mut paths = vec![("data", crate::state_dir())];
                let projects = store.lock().list_projects().unwrap_or_default();
                paths.extend(
                    projects
                        .into_iter()
                        .map(|p| ("worktree", std::path::PathBuf::from(p.path))),
                );
                check_disk_space(&paths);
                std::thread::sleep(PROBE_INTERVAL);
            }
        });
    if let Err(e) = spawned {
        // ralphus[ignore-rlog-pair]: thread spawn failure at startup; no entity to record against
        crate::rlog!(
            ERROR,
            "ralphus [health] could not start the probe thread: {e}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn emit_without_an_emitter_is_a_noop() {
        // OpenTelemetry disabled path: nothing started, nothing happens.
        emit(HealthEvent::ok("daemon.heartbeat"));
        flush(Duration::from_millis(10));
    }

    #[test]
    fn a_full_queue_drops_instead_of_blocking() {
        let (tx, _rx) = sync_channel(1);
        assert!(try_send(&tx, HealthEvent::ok("a")).is_ok());
        let started = Instant::now();
        assert!(try_send(&tx, HealthEvent::ok("b")).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_enabled_queue_delivers_events_in_order() {
        let (tx, rx) = sync_channel(4);
        try_send(&tx, HealthEvent::ok("a").attr("k", 1_i64)).unwrap();
        try_send(&tx, HealthEvent::error("b", "bad")).unwrap();
        assert_eq!(rx.recv().unwrap().name, "a");
        let second = rx.recv().unwrap();
        assert_eq!(second.name, "b");
        assert_eq!(second.error.as_deref(), Some("bad"));
    }

    #[test]
    fn deliver_builds_a_span_without_a_configured_exporter() {
        deliver(&HealthEvent::error("daemon.test", "x").attr("k", "v"));
    }

    #[test]
    fn disk_thresholds_split_at_ten_and_five_percent() {
        assert_eq!(classify_free_percent(50.0), None);
        assert_eq!(classify_free_percent(10.0), None);
        assert_eq!(classify_free_percent(9.9), Some(DiskSeverity::Warning));
        assert_eq!(classify_free_percent(5.0), Some(DiskSeverity::Warning));
        assert_eq!(classify_free_percent(4.9), Some(DiskSeverity::Critical));
    }

    #[test]
    fn volume_lookup_prefers_the_longest_mount_prefix() {
        let root = std::env::temp_dir();
        let nested = root.join("ralphus_volume_lookup_probe");
        std::fs::create_dir_all(&nested).unwrap();
        let canonical = nested.canonicalize().unwrap();
        let text = canonical.to_string_lossy();
        let stripped = PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text));
        let parent = stripped.parent().unwrap().to_path_buf();
        let volumes: Vec<Volume> = vec![
            (
                PathBuf::from(stripped.components().next().unwrap().as_os_str()),
                1,
                100,
            ),
            (parent.clone(), 2, 100),
        ];
        let found = volume_for_path(&volumes, &nested).expect("a volume");
        assert_eq!(found.0, parent);
        let _ = std::fs::remove_dir(&nested);
        assert!(volume_for_path(&[], Path::new("/nowhere")).is_none());
    }

    #[test]
    fn sqlite_errors_are_classified_by_code() {
        let err = |code| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code,
                    extended_code: 0,
                },
                None,
            )
        };
        use rusqlite::ErrorCode as C;
        assert_eq!(
            classify_sqlite_error(&err(C::DatabaseBusy)),
            Some(SqliteTrouble::Busy)
        );
        assert_eq!(
            classify_sqlite_error(&err(C::DiskFull)),
            Some(SqliteTrouble::WriteFailure)
        );
        assert_eq!(classify_sqlite_error(&err(C::ConstraintViolation)), None);
        assert_eq!(
            classify_sqlite_error(&rusqlite::Error::QueryReturnedNoRows),
            None
        );
    }

    #[test]
    fn sqlite_spans_are_rate_limited() {
        let last = std::sync::atomic::AtomicI64::new(0);
        assert!(sqlite_emit_due(&last, 100_000));
        assert!(!sqlite_emit_due(&last, 100_001));
        assert!(sqlite_emit_due(&last, 100_000 + SQLITE_EMIT_GAP_MS));
    }

    #[test]
    fn thread_count_fires_only_on_a_state_change() {
        let high = AtomicBool::new(false);
        assert!(thread_count_transition(&high, 10).is_none());
        let up = thread_count_transition(&high, THREAD_COUNT_WARN).expect("goes high");
        assert_eq!(up.name, "daemon.resource.threads_high");
        assert!(thread_count_transition(&high, THREAD_COUNT_WARN + 1).is_none());
        let down = thread_count_transition(&high, 10).expect("recovers");
        assert_eq!(down.name, "daemon.resource.threads_normal");
    }
}
