//! RAL-155: the unified, chronological "uber-log-viewer" for a whole Squad.
//!
//! Merges everything Cartographer already knows about a squad — state
//! transitions (`Store::set_squad_state`/`set_task_state`/`set_cell_state`/
//! `set_proof_state` all already emit Cartographer rows, see
//! `AGENTS.md`'s Logging Policy) and every other Cartographer event scoped to
//! it — with the durable per-attempt terminal-log content referenced by
//! `crate::terminal_log`-writing rows (see [`crate::cartographer::Note::log_path`]),
//! into one time-ordered narrative. "Task-Run" in the ticket's title turned
//! out to mean the whole Squad (all its tasks/cells/proofs), not a
//! separate entity — see the ticket's Q1.
//!
//! **Ordering / tie-break rule** (the ticket's Risk #1): entries sort by
//! `(at_ms, id)` ascending — Cartographer's own primary key, which is
//! monotonically increasing insertion order — so two rows sharing a
//! millisecond still sort deterministically and in emission order. A
//! terminal-log excerpt is anchored at its owning Cartographer row's `at_ms`
//! (when the attempt file was last written), not reconstructed line-by-line —
//! tmux pane captures carry no reliable per-line timestamps, so this is a
//! documented, deliberate approximation rather than an attempt at more
//! precision than the data supports.
//!
//! **Pruning gaps** (Risk #3): `Store::insert_squad` always logs a `"squad
//! inserted"` Cartographer row synchronously at submit time, so its absence
//! from a squad's returned rows is a reliable signal that Cartographer's
//! retention pruning (`cartographer_prune`) has already removed some of this
//! squad's earlier history. [`SquadTimelineMeta::gaps_possible`] surfaces that
//! rather than silently presenting a partial timeline as complete (Q6:
//! best-effort, no obligation to reconstruct pruned history).
//!
//! **Volume caps** (Risk #2 / Q4: no known event-volume scale, so use
//! conservative defaults): [`MAX_EVENTS`] bounds how many Cartographer rows
//! one timeline pulls in, and [`MAX_LOG_EXCERPT_LINES`] bounds how much of
//! each referenced terminal-log file is inlined, so one long-running or
//! multi-cell squad can't produce an unusably large merged file.
//!
//! The generated file is a temp artifact (Q5), rewritten on every call under
//! a fixed per-squad path in the OS temp directory — never intended to persist
//! long-term.

use serde::Serialize;

use crate::cartographer::CartographerFilter;
use crate::store::{Result, Store};

/// Conservative cap on how many Cartographer rows one timeline pulls in.
/// `crate::cartographer::cartographer_query` itself clamps a single page to
/// 1000, so this is paginated internally (see [`build_squad_timeline`]).
const MAX_EVENTS: i64 = 2000;
/// Page size for the internal pagination loop.
const PAGE_SIZE: i64 = 500;
/// How much of a referenced terminal-log file to inline per entry (the tail —
/// most recent output — same convention as `crate::terminal_log::write_attempt`).
const MAX_LOG_EXCERPT_LINES: usize = 200;

/// Metadata describing one generated timeline (RAL-155 AC: "structured JSON
/// ... including relevant metadata (squad id, task id, time range, source
/// counts)").
#[derive(Debug, Clone, Serialize)]
pub struct SquadTimelineMeta {
    pub squad_id: String,
    /// When this timeline was generated (Unix epoch milliseconds).
    pub generated_at_ms: i64,
    /// Earliest entry's `at_ms`, if any.
    pub start_ms: Option<i64>,
    /// Latest entry's `at_ms`, if any.
    pub end_ms: Option<i64>,
    /// Total Cartographer rows included (after the [`MAX_EVENTS`] cap).
    pub event_count: i64,
    /// How many of those rows reference a terminal-log file.
    pub terminal_log_count: i64,
    /// Task count from the squad's current structure (not derived from the
    /// possibly-pruned event history).
    pub task_count: i64,
    /// Cell count from the squad's current structure.
    pub cell_count: i64,
    /// `true` if [`MAX_EVENTS`] was hit — the timeline is a prefix, not the
    /// full history.
    pub truncated: bool,
    /// `true` if the squad's own `"squad inserted"` inaugural Cartographer row is
    /// missing from the returned rows, meaning retention pruning has already
    /// removed some of this squad's earlier history (best-effort, Q6).
    pub gaps_possible: bool,
}

/// One entry in the merged timeline.
#[derive(Debug, Clone, Serialize)]
pub struct SquadTimelineEntry {
    pub at_ms: i64,
    pub level: String,
    pub source: String,
    pub scope: Option<String>,
    pub task: Option<String>,
    pub cell_id: Option<String>,
    pub message: String,
    /// Path to a referenced terminal-log file, if this entry has one.
    pub log_path: Option<String>,
    /// The tail of that file's content (bounded by [`MAX_LOG_EXCERPT_LINES`]),
    /// inlined here so a consumer doesn't need a second round-trip. `None`
    /// when `log_path` is `None`, or the file was unreadable (already pruned,
    /// moved, etc. — best-effort).
    pub log_excerpt: Option<String>,
}

/// A generated, merged chronological view of one squad.
#[derive(Debug, Clone, Serialize)]
pub struct SquadTimeline {
    pub meta: SquadTimelineMeta,
    pub entries: Vec<SquadTimelineEntry>,
    /// The same content as `entries`, rendered as plain text — identical to
    /// what was written to `file_path`.
    pub text: String,
    /// Where the rendered text was (best-effort) written on disk, so the
    /// board's "generate to file, then display" button has something to
    /// point at. A temp artifact (RAL-155 Q5) — not guaranteed to persist.
    pub file_path: String,
}

/// Build the merged, chronological timeline for `squad_id`. `NotFound` if the
/// squad doesn't exist (mirrors every other squad-scoped `Store` accessor).
pub fn build_squad_timeline(store: &Store, squad_id: &str) -> Result<SquadTimeline> {
    let squad = store.get_squad(squad_id)?;
    let cell_count: i64 = squad.tasks.iter().map(|t| t.cells.len() as i64).sum();

    let mut rows = Vec::new();
    let mut offset = 0i64;
    let total = loop {
        let filter = CartographerFilter {
            squad_id: Some(squad_id.to_string()),
            limit: PAGE_SIZE,
            offset,
            ascending: true,
            ..CartographerFilter::default()
        };
        let page = store.cartographer_query(&filter)?;
        let total = page.total;
        let got = page.rows.len() as i64;
        rows.extend(page.rows);
        offset += PAGE_SIZE;
        if got < PAGE_SIZE || rows.len() as i64 >= MAX_EVENTS || offset >= total {
            break total;
        }
    };
    let gaps_possible = !rows.iter().any(|r| r.message == "squad inserted");
    let truncated = (rows.len() as i64) < total;
    rows.truncate(MAX_EVENTS as usize);

    let terminal_log_count = rows.iter().filter(|r| r.log_path.is_some()).count() as i64;
    let start_ms = rows.first().map(|r| r.at_ms);
    let end_ms = rows.last().map(|r| r.at_ms);

    let entries: Vec<SquadTimelineEntry> = rows
        .into_iter()
        .map(|r| {
            let log_excerpt = r.log_path.as_deref().and_then(read_log_excerpt);
            SquadTimelineEntry {
                at_ms: r.at_ms,
                level: r.level,
                source: r.source,
                scope: r.scope,
                task: r.task,
                cell_id: r.cell_id,
                message: r.message,
                log_path: r.log_path,
                log_excerpt,
            }
        })
        .collect();

    let generated_at_ms = crate::store::now_ms();
    let meta = SquadTimelineMeta {
        squad_id: squad_id.to_string(),
        generated_at_ms,
        start_ms,
        end_ms,
        event_count: entries.len() as i64,
        terminal_log_count,
        task_count: squad.tasks.len() as i64,
        cell_count,
        truncated,
        gaps_possible,
    };

    let text = render_text(&meta, &entries);
    let file_path = write_temp_file(squad_id, &text);

    Ok(SquadTimeline {
        meta,
        entries,
        text,
        file_path,
    })
}

/// The current-attempt-only, chronologically-merged debug stream backing
/// both the Live View pane's "Show Debug Messages" toggle and the "Open
/// Terminal Log" attempt-history popup (RAL-296), for one cell, proof step,
/// guardian branch resolver, or guardian manual-checks run -- the four
/// entity kinds that already share the exact `(squad_id, task, cell_id)` key
/// convention their tmux session naming uses (see
/// `server.rs::terminal_log_attempts_reply`'s doc comment, and this
/// function's own callers in `server.rs`, one per kind).
///
/// Reuses [`build_squad_timeline`]'s own merge mechanism -- the same
/// paginated Cartographer fetch, [`read_log_excerpt`] inlining, and
/// ordering/volume-cap rules -- scoped down to one entity via the query
/// filter directly, rather than fetching the whole squad's rows and
/// filtering client-side. Unlike `build_squad_timeline`, this also trims to
/// the most recent attempt: every runner/scheduler lifecycle event for any
/// of these four entity kinds is logged around a `"tmux session started"`
/// row (`Runner::emit_tmux_note`, entity-kind-agnostic -- see
/// `runner.rs::run_via_tmux`), so the *last* such row's position is a
/// reliable, kind-agnostic "this attempt started here" boundary. A detach +
/// restart cycle's earlier attempt(s) are trimmed off rather than stitched
/// in, matching the ticket's "current-attempt-only" design decision --
/// browsing *past* attempts remains the separate Attempt History box's job
/// (`terminal_log_attempts_reply`), unaffected by this trim.
pub fn entity_debug_timeline(
    store: &Store,
    squad_id: &str,
    task: &str,
    cell_id: &str,
) -> Result<Vec<SquadTimelineEntry>> {
    let mut rows = Vec::new();
    let mut offset = 0i64;
    loop {
        let filter = CartographerFilter {
            squad_id: Some(squad_id.to_string()),
            task: Some(task.to_string()),
            cell_id: Some(cell_id.to_string()),
            limit: PAGE_SIZE,
            offset,
            ascending: true,
            ..CartographerFilter::default()
        };
        let page = store.cartographer_query(&filter)?;
        let got = page.rows.len() as i64;
        let total = page.total;
        rows.extend(page.rows);
        offset += PAGE_SIZE;
        if got < PAGE_SIZE || rows.len() as i64 >= MAX_EVENTS || offset >= total {
            break;
        }
    }
    rows.truncate(MAX_EVENTS as usize);

    let attempt_start = rows
        .iter()
        .rposition(|r| r.message == "tmux session started")
        .unwrap_or(0);
    rows.drain(..attempt_start);

    Ok(rows
        .into_iter()
        .map(|r| {
            let log_excerpt = r.log_path.as_deref().and_then(read_log_excerpt);
            SquadTimelineEntry {
                at_ms: r.at_ms,
                level: r.level,
                source: r.source,
                scope: r.scope,
                task: r.task,
                cell_id: r.cell_id,
                message: r.message,
                log_path: r.log_path,
                log_excerpt,
            }
        })
        .collect())
}

/// Read the tail of a referenced terminal-log file, or `None` if it's
/// unreadable (already pruned, moved, permissions — best-effort, never an
/// error).
fn read_log_excerpt(path: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    // RAL-247: this file is read directly from disk (not via `read_attempt`),
    // so scrub credential values here too — a legacy file written before the
    // write-time redaction could otherwise surface in the served timeline.
    let redacted = ralphus_core::redact::redact_secrets(&content);
    Some(crate::runner::tail_lines(&redacted, MAX_LOG_EXCERPT_LINES))
}

fn render_text(meta: &SquadTimelineMeta, entries: &[SquadTimelineEntry]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "=== ralphus uber-log timeline: squad {} ===\n",
        meta.squad_id
    ));
    out.push_str(&format!(
        "generated={} events={} (truncated={}) terminal_logs={} tasks={} cells={}\n",
        format_ts(meta.generated_at_ms),
        meta.event_count,
        meta.truncated,
        meta.terminal_log_count,
        meta.task_count,
        meta.cell_count,
    ));
    if meta.gaps_possible {
        out.push_str(
            "NOTE: this squad's earliest Cartographer history appears to have been pruned \
             (retention_days/max_rows) — this timeline is best-effort, not guaranteed complete.\n",
        );
    }
    out.push('\n');
    for entry in entries {
        out.push_str(&render_entry(entry));
        out.push('\n');
    }
    out
}

fn render_entry(entry: &SquadTimelineEntry) -> String {
    let mut ctx = Vec::new();
    if let Some(scope) = &entry.scope {
        ctx.push(format!("scope={scope}"));
    }
    if let Some(task) = &entry.task {
        ctx.push(format!("task={task}"));
    }
    if let Some(sid) = &entry.cell_id {
        ctx.push(format!("cell={sid}"));
    }
    let ctx_str = if ctx.is_empty() {
        String::new()
    } else {
        format!(" [{}]", ctx.join(" "))
    };
    let mut out = format!(
        "[{}] {:<7} {:<12} {}{}",
        format_ts(entry.at_ms),
        entry.level.to_uppercase(),
        entry.source,
        entry.message,
        ctx_str,
    );
    if let Some(excerpt) = &entry.log_excerpt {
        if let Some(path) = &entry.log_path {
            out.push_str(&format!("\n    (terminal log: {path})"));
        }
        for line in excerpt.lines() {
            out.push_str("\n    | ");
            out.push_str(line);
        }
    }
    out
}

fn format_ts(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| at_ms.to_string())
}

/// Write the rendered timeline to a fixed, per-squad path in the OS temp
/// directory (RAL-155 Q5: a temp artifact, rewritten on every generation, not
/// required to persist long-term). Best-effort: a write failure is logged but
/// never fails the request — the caller still gets `text`/`entries` back.
///
/// Writes to a unique-per-call sibling path first and renames it into place,
/// rather than writing `path` directly — `std::fs::write` is not atomic, so
/// two overlapping generations for the same squad (e.g. two quick "Timeline"
/// clicks) could otherwise interleave and leave a reader observing a torn or
/// empty file. Rename is atomic on both POSIX and Windows.
fn write_temp_file(squad_id: &str, text: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!("ralphus-timeline-{squad_id}.log"));
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = std::env::temp_dir().join(format!(
        "ralphus-timeline-{squad_id}.{}.{unique}.tmp",
        std::process::id()
    ));
    if let Err(e) = std::fs::write(&tmp_path, text).and_then(|()| std::fs::rename(&tmp_path, &path))
    {
        let _ = std::fs::remove_file(&tmp_path);
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            WARNING,
            "ralphus [timeline] could not write {}: {e}",
            path.display()
        );
    }
    path.to_string_lossy().into_owned()
}

/// Every `Store::open_in_memory()` used in tests starts its squad-id sequence
/// over from `squad-000000000001`, so two tests that each seed a fresh store
/// collide on the exact same `write_temp_file` OS path when cargo runs them
/// concurrently (the default). Any test that calls `build_squad_timeline` --
/// here or in `server.rs`'s matching route test -- must hold this lock for
/// its duration so those writes (and any read-back of the resulting file)
/// never interleave with one another.
#[cfg(test)]
pub(crate) static TIMELINE_FILE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cartographer::Note;

    const SAMPLE: &str = r#"
[[task]]
name = "build"
[[task.cell]]
id = "worker"
cwd = "/repo"
prompt = "make it build"
"#;

    fn seeded_squad() -> (Store, String) {
        let mut store = Store::open_in_memory().unwrap();
        let file: ralphus_core::schema::TaskFile = toml::from_str(SAMPLE).expect("valid toml");
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        (store, squad_id)
    }

    #[test]
    fn unknown_squad_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        assert!(build_squad_timeline(&store, "nope").is_err());
    }

    #[test]
    fn includes_the_squad_inserted_event_and_reports_no_gaps() {
        let _guard = TIMELINE_FILE_TEST_LOCK.lock().unwrap();
        let (store, squad_id) = seeded_squad();
        let timeline = build_squad_timeline(&store, &squad_id).unwrap();
        assert!(!timeline.meta.gaps_possible);
        assert!(
            timeline
                .entries
                .iter()
                .any(|e| e.message == "squad inserted")
        );
        assert_eq!(timeline.meta.task_count, 1);
        assert_eq!(timeline.meta.cell_count, 1);
        assert!(timeline.text.contains(&squad_id));
    }

    #[test]
    fn detects_pruning_gaps_when_the_inaugural_event_is_missing() {
        let _guard = TIMELINE_FILE_TEST_LOCK.lock().unwrap();
        let (store, squad_id) = seeded_squad();
        // Simulate retention pruning having removed the earliest row.
        store.cartographer_prune(0, 0).ok(); // no-op caps, seed more first
        Note::new("scheduler")
            .squad(&squad_id)
            .emit(&store, "later event", serde_json::json!({}));
        // Prune down to just the newest row.
        store.cartographer_prune(0, 1).unwrap();
        let timeline = build_squad_timeline(&store, &squad_id).unwrap();
        assert!(timeline.meta.gaps_possible);
    }

    #[test]
    fn inlines_terminal_log_excerpt_from_log_path() {
        let _guard = TIMELINE_FILE_TEST_LOCK.lock().unwrap();
        let (store, squad_id) = seeded_squad();
        let dir =
            std::env::temp_dir().join(format!("ralphus-timeline-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("attempt.log");
        std::fs::write(&log_file, "hello from the pane\nsecond line").unwrap();

        Note::new("runner")
            .squad(&squad_id)
            .scope("terminal_log")
            .log_path(log_file.to_str().unwrap())
            .emit(
                &store,
                "terminal log attempt written",
                serde_json::json!({}),
            );

        let timeline = build_squad_timeline(&store, &squad_id).unwrap();
        let entry = timeline
            .entries
            .iter()
            .find(|e| e.log_path.as_deref() == Some(log_file.to_str().unwrap()))
            .expect("terminal_log entry present");
        assert_eq!(
            entry.log_excerpt.as_deref(),
            Some("hello from the pane\nsecond line")
        );
        assert!(timeline.text.contains("hello from the pane"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_log_file_is_best_effort_none() {
        let _guard = TIMELINE_FILE_TEST_LOCK.lock().unwrap();
        let (store, squad_id) = seeded_squad();
        Note::new("runner")
            .squad(&squad_id)
            .scope("terminal_log")
            .log_path("/does/not/exist.log")
            .emit(
                &store,
                "terminal log attempt written",
                serde_json::json!({}),
            );

        let timeline = build_squad_timeline(&store, &squad_id).unwrap();
        let entry = timeline
            .entries
            .iter()
            .find(|e| e.log_path.as_deref() == Some("/does/not/exist.log"))
            .expect("terminal_log entry present");
        assert_eq!(entry.log_excerpt, None);
    }

    #[test]
    fn writes_a_temp_file_containing_the_rendered_text() {
        let _guard = TIMELINE_FILE_TEST_LOCK.lock().unwrap();
        let (store, squad_id) = seeded_squad();
        let timeline = build_squad_timeline(&store, &squad_id).unwrap();
        let on_disk = std::fs::read_to_string(&timeline.file_path).unwrap();
        assert_eq!(on_disk, timeline.text);
    }

    #[test]
    fn entries_are_sorted_ascending_by_at_ms_then_id() {
        let _guard = TIMELINE_FILE_TEST_LOCK.lock().unwrap();
        let (store, squad_id) = seeded_squad();
        for i in 0..5 {
            Note::new("scheduler").squad(&squad_id).emit(
                &store,
                format!("event {i}"),
                serde_json::json!({}),
            );
        }
        let timeline = build_squad_timeline(&store, &squad_id).unwrap();
        let at_ms: Vec<i64> = timeline.entries.iter().map(|e| e.at_ms).collect();
        let mut sorted = at_ms.clone();
        sorted.sort_unstable();
        assert_eq!(at_ms, sorted);
    }

    #[test]
    fn entity_debug_timeline_scopes_to_one_task_cell_pair() {
        let (store, squad_id) = seeded_squad();
        Note::new("scheduler")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .emit(&store, "cell start", serde_json::json!({}));
        Note::new("scheduler")
            .squad(&squad_id)
            .task("build")
            .cell("other-cell")
            .emit(&store, "cell start (other)", serde_json::json!({}));
        let entries = entity_debug_timeline(&store, &squad_id, "build", "worker").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "cell start");
    }

    #[test]
    fn entity_debug_timeline_trims_to_the_most_recent_attempt() {
        let (store, squad_id) = seeded_squad();
        // First attempt: starts, runs, ends.
        Note::new("scheduler")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .emit(&store, "cell start", serde_json::json!({}));
        Note::new("runner")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .emit(&store, "tmux session started", serde_json::json!({}));
        Note::new("runner")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .emit(&store, "invoked", serde_json::json!({}));
        // Detach + restart: a second attempt begins.
        Note::new("scheduler")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .emit(
                &store,
                "cell detached for manual takeover",
                serde_json::json!({}),
            );
        Note::new("scheduler")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .emit(&store, "cell start", serde_json::json!({}));
        Note::new("runner")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .emit(&store, "tmux session started", serde_json::json!({}));
        Note::new("runner")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .emit(&store, "session-id known", serde_json::json!({}));

        let entries = entity_debug_timeline(&store, &squad_id, "build", "worker").unwrap();
        let messages: Vec<&str> = entries.iter().map(|e| e.message.as_str()).collect();
        // Only the second attempt's own boundary onward -- the first
        // attempt's "cell start"/"invoked" and the intervening detach
        // event are trimmed off, not stitched into the same stream.
        assert_eq!(messages, vec!["tmux session started", "session-id known"]);
    }

    #[test]
    fn entity_debug_timeline_inlines_terminal_log_excerpt() {
        let (store, squad_id) = seeded_squad();
        let dir = std::env::temp_dir().join(format!(
            "ralphus-entity-timeline-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("attempt.log");
        std::fs::write(&log_file, "hello from the pane").unwrap();

        Note::new("runner")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .emit(&store, "tmux session started", serde_json::json!({}));
        Note::new("runner")
            .squad(&squad_id)
            .task("build")
            .cell("worker")
            .scope("terminal_log")
            .log_path(log_file.to_str().unwrap())
            .emit(
                &store,
                "terminal log attempt 0 written",
                serde_json::json!({}),
            );

        let entries = entity_debug_timeline(&store, &squad_id, "build", "worker").unwrap();
        let written = entries
            .iter()
            .find(|e| e.message == "terminal log attempt 0 written")
            .expect("terminal log entry present");
        assert_eq!(written.log_excerpt.as_deref(), Some("hello from the pane"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
