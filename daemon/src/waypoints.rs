//! Cross-squad waypoints (RAL-400) -- schema/store layer (Phase 1) plus the
//! survey pass (Phase 2), squad gating (Phase 3), review-feedback delivery
//! (Phase 4), and lifecycle (Phase 6).
//!
//! A waypoint is a named, open/closed join point that tracks a affected of
//! reviews/squads and accumulates append-only guidance ("bearings") for
//! them. This module owns the affected/bearing/injection CRUD, the
//! terminal-state auto-close computation, the survey (the LLM pass that
//! decides, for every open review/non-terminal squad whose [`Scope`]
//! overlaps a waypoint's, whether it is impacted and at what [`AffectedMode`]),
//! and delivery: [`run_pending_deliveries`] pushes an impacted review-kind
//! affected entry's guidance into its review worktree via the existing
//! `guardian_merge::start_feedback` path, and marks a squad-kind entry
//! whose squad finished before any review ever formed for it `via-restack`.
//! A separate `pending_injections` mechanism
//! ([`PendingInjectionView`]) is schema-only -- see
//! `.agent/waypoints-phase0-decisions.md` for the full design.
//!
//! Lifecycle (Phase 6): [`Store::maybe_auto_close_waypoint`] closes a
//! waypoint once every affected entry has reached a terminal state (a squad's
//! `done`/`cancelled`, per [`crate::store::SquadState::is_terminal_for_waypoint`];
//! a review's `merged`/`cancelled`/`deployed`, per
//! [`GuardianStatus::is_terminal_status`] -- both deliberately excluding
//! `failed`/`merge_failed`, which may still be retried), hooked into
//! `Store::set_squad_state`/`Store::set_guardian_status` via
//! [`Store::maybe_auto_close_waypoints_for_affected_entry`] right after either
//! transition lands. [`Store::close_waypoint_manually`]/
//! [`Store::reopen_waypoint`] are the store-level primitives for manual
//! close/reopen (HTTP, CLI and MCP surfaces all exist); a manual close takes
//! effect for gating immediately, since `Store::squad_block_gating_waypoint`
//! filters on live `state='open'` with no extra plumbing needed. Closing a
//! waypoint (auto or manual) queues an optional one-time stand-down notice
//! for each *advisory*-mode affected entry (`Block`-mode entries get none --
//! gating simply lifting is itself the signal); [`run_pending_stand_down_notices`]
//! is the scheduler sweep that sends them and marks each entry's
//! `stand_down_at_ms` so a later waypoint reopen+reclose never re-sends one.
//!
//! Phase 5 (scenario 3, in-flight delivery + parking) pulls forward only the
//! hard-halt/ghost-fold half of the design; see
//! `.agent/waypoints-phase5-decisions.md`. When a squad's in-flight cell is
//! halted for a blocking waypoint (`daemon/src/scheduler.rs`'s
//! `is_waypoint_halted()` branch), the halted cell's current bearing list is
//! rendered via [`render_bearing_block`] and folded into that cell's own
//! ghost note (`Store::upsert_ghost`), so the existing ghost-context prepend
//! to a cell's prompt on redispatch (already built for dependency handoffs)
//! carries it forward once the waypoint closes and the cell resumes -- no
//! new delivery channel, no new `CellSpec`/`RunnerSpec` field. The static,
//! unconditional half of the waypoint-injection prompt contract (what a
//! bearing block means, and that the agent must inspect its own working
//! state rather than assume) lives instead in `daemon/src/runner.rs`'s
//! `WAYPOINT_SYSTEM_PROMPT`, mirrored byte-identically in
//! `runner/src/execute.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::chat_client::{self, ChatMessage};
use crate::guardian::GuardianStatus;
use crate::runner::Runner;
use crate::store::{Result as StoreResult, Store, StoreError, now_ms};
use crate::triage::SubprojectResolution;

/// Unwraps a store result whose failure a waypoint sweep or notification
/// tolerates (skipping, defaulting, or carrying on), recording the failure as
/// a WARNING `waypoints` row first so it is not lost.
fn log_swallowed<T>(store: &Store, context: &str, result: StoreResult<T>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(e) => {
            crate::cartographer::Note::new("waypoints")
                .level(crate::logging::LogLevel::WARNING)
                .scope("waypoint")
                .emit(
                    store,
                    format!("{context} failed: {e}"),
                    serde_json::json!({ "context": context, "error": e.to_string() }),
                );
            None
        }
    }
}

/// One `waypoint_roster` row with the state of the squad or review it names,
/// as read by [`Store::block_gated_squads`]. The state of an entry that does
/// not exist (or of the other kind) is `None`.
#[derive(Debug, Clone)]
struct RosterRow {
    waypoint_id: String,
    kind: String,
    squad_state: Option<String>,
    review_status: Option<String>,
}

impl RosterRow {
    /// Same reading as `Store::entry_work_is_terminal`: a missing squad or
    /// review is non-terminal, and an unknown kind is not a roster entry.
    fn terminal(&self) -> Option<bool> {
        Some(match WaypointEntryKind::parse(&self.kind)? {
            WaypointEntryKind::Squad => self
                .squad_state
                .as_deref()
                .and_then(crate::store::SquadState::parse)
                .is_some_and(crate::store::SquadState::is_terminal_for_waypoint),
            WaypointEntryKind::Review => self
                .review_status
                .as_deref()
                .is_some_and(GuardianStatus::is_terminal_status),
        })
    }
}

/// The squads held by at least one candidate waypoint whose roster is not
/// fully terminal. `candidates` are `(squad_id, waypoint_id)` pairs; a
/// waypoint with an empty roster never holds anything.
fn gated_squads_in(candidates: &[(String, String)], roster: &[RosterRow]) -> BTreeSet<String> {
    let unfinished: BTreeSet<&str> = roster
        .iter()
        .filter(|row| row.terminal() == Some(false))
        .map(|row| row.waypoint_id.as_str())
        .collect();
    candidates
        .iter()
        .filter(|(_, waypoint_id)| unfinished.contains(waypoint_id.as_str()))
        .map(|(squad_id, _)| squad_id.clone())
        .collect()
}

/// A waypoint/review/squad's aggregate monorepo-subproject footprint (the
/// Phase 0 actionable-notification matching model, see
/// `.agent/waypoints-phase0-decisions.md`). Consumed by
/// [`Store::waypoint_survey_candidates`] to pick the narrowest candidate set
/// before any survey call is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// At least one contributing cell couldn't be narrowed to a concrete
    /// subproject set (not a monorepo, or resolution just hasn't run yet).
    /// Contagious under union: a false negative (silently skipping a
    /// candidate that should have been surveyed) is unacceptable, while a
    /// false positive only costs one extra survey call.
    RepoWide,
    /// The union of every contributing cell's resolved subproject set.
    Areas(BTreeSet<String>),
}

/// Aggregate a set of per-cell subproject resolutions into one [`Scope`]:
/// `RepoWide` if any cell is `NotApplicable`/`Unresolved`, else the union of
/// every `Resolved` cell's subproject set.
#[must_use]
pub fn aggregate_scope(cells: &[SubprojectResolution]) -> Scope {
    let mut areas = BTreeSet::new();
    for cell in cells {
        match cell {
            SubprojectResolution::NotApplicable | SubprojectResolution::Unresolved => {
                return Scope::RepoWide;
            }
            SubprojectResolution::Resolved { subprojects, .. } => {
                areas.extend(subprojects.iter().cloned());
            }
        }
    }
    Scope::Areas(areas)
}

/// Merge two scopes for the same project. `RepoWide` wins (contagious),
/// matching [`aggregate_scope`]'s own false-negatives-unacceptable rule.
fn union_scope(a: &Scope, b: &Scope) -> Scope {
    match (a, b) {
        (Scope::RepoWide, _) | (_, Scope::RepoWide) => Scope::RepoWide,
        (Scope::Areas(x), Scope::Areas(y)) => Scope::Areas(x.union(y).cloned().collect()),
    }
}

/// Whether two scopes for the *same project* overlap: `RepoWide` on either
/// side always overlaps (it's the conservative "could be anything" case,
/// same rationale as [`aggregate_scope`]'s contagion rule); two `Areas` sets
/// overlap iff they share at least one subproject name.
#[must_use]
fn scopes_overlap(a: &Scope, b: &Scope) -> bool {
    match (a, b) {
        (Scope::RepoWide, _) | (_, Scope::RepoWide) => true,
        (Scope::Areas(x), Scope::Areas(y)) => !x.is_disjoint(y),
    }
}

/// Which kind of entity a affected entry or bearing producer refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaypointEntryKind {
    Review,
    Squad,
}

impl WaypointEntryKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Squad => "squad",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "review" => Self::Review,
            "squad" => Self::Squad,
            _ => return None,
        })
    }
}

/// Whether a affected entry's waypoint guidance is a hard gate (`block`,
/// delivery is required) or informational (`advisory`, delivery is
/// best-effort). Only meaningful on waypoints with `allow_advisory = true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AffectedMode {
    Block,
    Advisory,
}

impl AffectedMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Advisory => "advisory",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "block" => Self::Block,
            "advisory" => Self::Advisory,
            _ => return None,
        })
    }
}

/// Whether a affected entry's waypoint guidance has reached it yet.
/// `via_restack` distinguishes delivery folded into an unrelated rebase from
/// a dedicated injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Undelivered,
    Delivered,
    ViaRestack,
    Failed,
}

impl DeliveryStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Undelivered => "undelivered",
            Self::Delivered => "delivered",
            Self::ViaRestack => "via-restack",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "undelivered" => Self::Undelivered,
            "delivered" => Self::Delivered,
            "via-restack" => Self::ViaRestack,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// One row of `waypoint_affected`: a unit of work this waypoint lands on,
/// with the survey's verdict about it, how its guidance was delivered, and
/// how it answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AffectedEntryView {
    pub waypoint_id: String,
    pub kind: WaypointEntryKind,
    pub entry_id: String,
    pub mode: AffectedMode,
    pub survey_verdict: Option<String>,
    pub survey_rationale: Option<String>,
    pub delivery_status: DeliveryStatus,
    /// Set once this entry's optional advisory stand-down notice (RAL-400
    /// Phase 6) has been sent -- `None` while still pending. See
    /// [`run_pending_stand_down_notices`].
    pub stand_down_at_ms: Option<i64>,
    /// Set once this entry's already-finished work was flagged as possibly
    /// needing a redo -- its waypoint closed while the survey had judged it
    /// `impacted`, so the work landed without the waypoint's own changes.
    /// `None` means not flagged. Purely advisory: nothing is re-run until
    /// someone calls [`redo_affected_entry`]. See [`run_pending_stale_notices`].
    pub stale_at_ms: Option<i64>,
    /// How this entry answered the waypoint, once it has. `None` means it has
    /// not -- which for a `block`-mode entry is exactly what holds it, and
    /// what keeps the waypoint out of phase 2. See [`BearingDecision`].
    pub bearing_decision: Option<BearingDecision>,
    pub bearing_decided_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One row of `waypoint_roster`: a review or squad whose landing *is* this
/// waypoint being carried out.
///
/// Deliberately thin next to [`AffectedEntryView`]. A goal carries no survey
/// verdict, no delivery status and no bearing, because none of those apply:
/// it is not work the waypoint lands on, it is work the waypoint consists of.
/// All it needs is to finish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RosterEntryView {
    pub waypoint_id: String,
    pub kind: WaypointEntryKind,
    pub entry_id: String,
    /// Why this is on the list, in whoever added it's own words.
    pub note: Option<String>,
    /// Whether this goal has reached a terminal state. Derived per read
    /// rather than stored, so it cannot go stale against the squad/review it
    /// describes.
    pub terminal: bool,
    pub created_at_ms: i64,
}

/// How a unit of affected work answered a waypoint's guidance.
///
/// A waypoint asks downstream work to do something. Whether it was done is
/// not something the daemon can observe -- a squad finishing proves it
/// stopped, not that it complied -- so the answer is stated, by the agent
/// that did the work, as a `RALPHUS_BEARING:` line. Declining is a valid
/// answer and is recorded as one; what is not valid is silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BearingDecision {
    /// The guidance was taken up in this work.
    Accepted,
    /// The guidance was considered and deliberately not taken up.
    Rejected,
    /// The guidance applies but was not acted on now.
    Deferred,
}

impl BearingDecision {
    /// The wire/storage spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Deferred => "deferred",
        }
    }

    /// Parses a decision, case-insensitively. A closed set: an agent writing
    /// anything else has not answered, and is treated as not having answered
    /// rather than having its prose stored as if it were a decision.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "accepted" => Some(Self::Accepted),
            "rejected" => Some(Self::Rejected),
            "deferred" => Some(Self::Deferred),
            _ => None,
        }
    }

    /// Every decision, for callers rendering the closed set.
    #[must_use]
    pub fn all() -> [Self; 3] {
        [Self::Accepted, Self::Rejected, Self::Deferred]
    }
}

/// One row of `waypoint_bearings`. Append-only: rows are created via
/// [`Store::append_waypoint_bearing`] and never updated or deleted, so `id`
/// (an autoincrement rowid) doubles as the stable arrival order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BearingView {
    pub id: i64,
    pub waypoint_id: String,
    pub producer_kind: WaypointEntryKind,
    pub producer_id: String,
    pub summary: String,
    pub entity_uri: Option<String>,
    pub commit_id: Option<String>,
    pub commit_summary: Option<String>,
    pub created_at_ms: i64,
}

/// One row of `pending_injections`: a queued piece of waypoint guidance for
/// one specific cell. Written by [`Store::queue_advisory_bearing_injections`]
/// when a bearing is appended, drained at cell dispatch, and rendered into
/// that cell's prompt by [`render_injection_block`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingInjectionView {
    pub id: i64,
    pub target_squad: String,
    pub target_task: i64,
    pub target_idx: i64,
    pub payload: String,
    pub status: String,
    pub batch_id: Option<String>,
    /// The waypoint whose guidance this injection carries. Lets a drained
    /// injection be attributed back to its waypoint without re-deriving it
    /// from the payload text -- which the consolidated waypoint event feed
    /// needs in order to report the delivery at all.
    pub waypoint_id: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One row of `waypoints`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaypointView {
    pub id: String,
    pub label: Option<String>,
    pub prompt: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub allow_advisory: bool,
    pub state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub closed_at_ms: Option<i64>,
}

/// One entry a re-survey would act on, as shown in the confirmation that
/// precedes one. Carries enough to name the entry and say what re-judging it
/// costs, without the caller needing a second lookup per row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResurveyTarget {
    pub kind: WaypointEntryKind,
    pub entry_id: String,
    /// The squad's label or the review's name; `None` when it has none.
    pub label: Option<String>,
    pub mode: String,
    pub current_verdict: Option<String>,
    pub delivery_status: String,
    /// Whether clearing this entry's verdict re-holds it until the classifier
    /// judges it again -- true for `block` mode, since the gate reads a NULL
    /// verdict as "not cleared".
    pub will_be_held_until_judged: bool,
}

/// What [`Store::clear_auto_enrolled_survey_verdicts`] would do, resolved
/// before it is done. See [`Store::resurvey_preview`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResurveyPreview {
    /// Entries a re-survey re-judges.
    pub targets: Vec<ResurveyTarget>,
    /// Human-declared entries a re-survey deliberately leaves alone.
    pub held_explicit: Vec<ResurveyTarget>,
}

/// A candidate identified by [`Store::waypoint_survey_candidates`]: an open
/// review or non-terminal squad not already an explicit affected entry, whose
/// own [`Scope`] overlaps the waypoint's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurveyCandidate {
    pub kind: WaypointEntryKind,
    pub entry_id: String,
}

/// The result of surveying one candidate: whether it was judged impacted,
/// under what mode, and why. Always populated -- including on a call
/// failure/timeout/unparseable reply, which fail closed to `impacted = true`,
/// `mode = Block` (see [`Store::survey_candidate`]'s doc comment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SurveyVerdict {
    pub impacted: bool,
    pub mode: AffectedMode,
    pub rationale: String,
}

/// Most effects one waypoint's feed returns. The query is now attribution-
/// filtered in SQL, so this bounds a single waypoint's own history rather than
/// how much of the Cartographer table gets scanned looking for it.
const WAYPOINT_MAX_EVENTS: i64 = 2000;

/// One entry in a waypoint's delivery/event history (RAL-400 Phase 7,
/// `GET /api/waypoints/{id}/deliveries`): a Cartographer row this module
/// itself emitted for the waypoint, oldest first.
#[derive(Debug, Clone, Serialize)]
pub struct WaypointEventEntry {
    pub at_ms: i64,
    pub level: String,
    pub message: String,
    pub payload: serde_json::Value,
    /// Which entity this effect landed on. Carried straight through from the
    /// Cartographer row's own refs, which is what turns a flat event list into
    /// a view of what a waypoint did *to each squad, cell and review* -- the
    /// reason to look at this feed at all.
    pub squad_id: Option<String>,
    pub guardian_id: Option<String>,
    pub cell_id: Option<String>,
    pub task: Option<String>,
    /// The subsystem that recorded the effect (`waypoints`, `scheduler`,
    /// `submit`, `server`). Worth surfacing because it distinguishes a
    /// decision the survey made from an action the scheduler took on it.
    pub source: String,
}

impl Store {
    /// Create a new open waypoint. Callers are responsible for generating
    /// `id` (mirrors every other entity id in this store -- see
    /// `crate::ids`).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn create_waypoint(
        &self,
        id: &str,
        label: Option<&str>,
        prompt: &str,
        agent: Option<&str>,
        model: Option<&str>,
        allow_advisory: bool,
    ) -> StoreResult<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO waypoints(id, label, prompt, agent, model, allow_advisory, state, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,?,?,'open',?,?)",
            params![id, label, prompt, agent, model, allow_advisory, now, now],
        )?;
        Ok(())
    }

    /// Create every `[[waypoint]]` block a submission declared (RAL-400
    /// Phase 8), resolving each affected entry's sentinel to a real id:
    /// `<<ralphus:new-squad>>` to this submission's own `squad_id`,
    /// `<<ralphus:new-review/<key>>>` to the guardian
    /// `crate::reviews::derive_reviews_with_full_prefetch` created for that
    /// same-file `[[review]]` block (via
    /// [`Store::guardian_id_for_review_key`]), and a plain
    /// `<<review:<id>>>`/`<<squad:<id>>>` literally. Deliberately mirrors
    /// `POST /api/waypoints`'s own permissiveness: neither path checks that a
    /// literal affected id names a real row, so a submission naming a typo'd
    /// id behaves the same as the HTTP API would (a dangling affected entry
    /// that never delivers, not a rejected submission). Callers must run
    /// this only after review derivation has already succeeded for the same
    /// `squad_id`, so every same-file `[[review]]` block's guardian row
    /// already exists.
    ///
    /// # Errors
    /// Propagates any SQLite failure, or [`StoreError::InvalidTransition`]
    /// if a same-file `<<ralphus:new-review/<key>>>` placeholder doesn't
    /// match any guardian created for this squad -- `core::validate`
    /// rejects an unmatched placeholder offline before the daemon ever sees
    /// it, so this indicates review derivation didn't actually create the
    /// review it reported success for.
    pub(crate) fn create_submission_waypoints(
        &self,
        squad_id: &str,
        waypoints: &[ralphus_core::schema::WaypointDef],
    ) -> StoreResult<()> {
        for w in waypoints {
            let id = self.next_id("waypoint_seq", "waypoint")?;
            self.create_waypoint(
                &id,
                w.label.as_deref(),
                &w.prompt,
                w.agent.as_deref(),
                w.model.as_deref(),
                w.allow_advisory,
            )?;
            for entry in &w.affected {
                // `core::validate`'s `validate_waypoint_blocks` already
                // rejects an unparseable affected entry offline -- a `None`
                // here would mean the daemon is running against a task file
                // that bypassed that check.
                let Some(parsed) = ralphus_core::schema::parse_affected_entry_sentinel(entry)
                else {
                    return Err(StoreError::InvalidTransition(format!(
                        "waypoint affected entry {entry:?} is not a valid sentinel"
                    )));
                };
                let (kind, entry_id) = match parsed {
                    ralphus_core::schema::AffectedEntryRef::NewSquad => {
                        (WaypointEntryKind::Squad, squad_id.to_string())
                    }
                    ralphus_core::schema::AffectedEntryRef::ExistingSquad(existing) => {
                        (WaypointEntryKind::Squad, existing)
                    }
                    ralphus_core::schema::AffectedEntryRef::ExistingReview(raw) => {
                        match ralphus_core::schema::review_link_key(&raw) {
                            Some(key) => match self.guardian_id_for_review_key(squad_id, key)? {
                                Some(guardian_id) => (WaypointEntryKind::Review, guardian_id),
                                None => {
                                    return Err(StoreError::InvalidTransition(format!(
                                        "waypoint affected entry references review key \
                                         {key:?}, but no guardian was created for it in \
                                         squad {squad_id}"
                                    )));
                                }
                            },
                            None => (WaypointEntryKind::Review, raw),
                        }
                    }
                };
                self.add_affected_entry(&id, kind, &entry_id, AffectedMode::Block)?;
            }
        }
        Ok(())
    }

    /// Fetch one waypoint by id.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if no such waypoint exists, or
    /// propagates any other SQLite failure.
    pub fn get_waypoint(&self, id: &str) -> StoreResult<WaypointView> {
        self.conn
            .query_row(
                "SELECT id, label, prompt, agent, model, allow_advisory, state, created_at_ms, updated_at_ms, closed_at_ms
                 FROM waypoints WHERE id=?",
                params![id],
                |r| {
                    Ok(WaypointView {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        prompt: r.get(2)?,
                        agent: r.get(3)?,
                        model: r.get(4)?,
                        allow_advisory: r.get(5)?,
                        state: r.get(6)?,
                        created_at_ms: r.get(7)?,
                        updated_at_ms: r.get(8)?,
                        closed_at_ms: r.get(9)?,
                    })
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// A squad's aggregate [`Scope`], per project. Groups every cell in the
    /// squad by its task's (already project/fallback-resolved) project
    /// name, then applies [`aggregate_scope`] within each project bucket --
    /// a cell with no recorded row in `cell_subprojects` is treated as
    /// `Unresolved` (RAL-346's binding decision: "nothing resolved yet" is
    /// never silently treated as an empty match).
    ///
    /// # Errors
    /// Propagates any SQLite failure, including the squad not existing.
    /// The project key a task's cells are bucketed under for waypoint scope
    /// matching: the **registered project** the cells live in, falling back to
    /// the task's own display project when no registered project contains them.
    ///
    /// `TaskView::project` is a *display* value (RAL-141): when a task sets no
    /// `project`, it degrades to the last path component of its first cell's
    /// `cwd`. Keying scope matching on that silently partitioned one registered
    /// repository by subdirectory -- a cell in `repo/core` landed under
    /// `"core"` while a cell in `repo` landed under `"repo"`, so a waypoint
    /// affecteded from one could not match genuinely impacted work from the
    /// other. That is a false negative against the ticket's inclusion boundary,
    /// and the silent bypass its Risks section names.
    ///
    /// Phase 0 specified this directly -- "project identity, via the existing
    /// `pool_key_for_path`/registered-project lookup" -- so this restores the
    /// documented design. `TaskView::project` is left alone everywhere it is
    /// used for display.
    fn scope_project_key(&self, cwd: Option<&str>, display_project: &str) -> String {
        cwd.filter(|c| !c.trim().is_empty())
            .and_then(|c| self.project_name_for_path(c))
            .unwrap_or_else(|| display_project.to_string())
    }

    /// A squad's aggregate [`Scope`], per project.
    ///
    /// Reads only the four columns scope matching actually needs -- each task's
    /// index and display project, and each cell's index and `cwd` -- rather than
    /// hydrating the squad through `get_squad`, which pulls every cell's prompt,
    /// command, env overrides, usage counters and agent settings to read two
    /// fields off them. This runs per affected entry and per survey candidate, and
    /// again for every open waypoint on submit, so the difference compounds.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn squad_scope_by_project(&self, squad_id: &str) -> StoreResult<BTreeMap<String, Scope>> {
        let resolved = Store::subprojects_by_cell(&self.conn, squad_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT t.idx, t.project, c.idx, c.cwd
             FROM tasks t JOIN cells c ON c.squad_id = t.squad_id AND c.task_idx = t.idx
             WHERE t.squad_id = ? ORDER BY t.idx, c.idx",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        // One project key per task, resolved from that task's first cell with a
        // cwd -- `project_name_for_path` reads the whole project table on every
        // call, so it is asked once per task rather than once per cell.
        let mut task_key: BTreeMap<i64, String> = BTreeMap::new();
        for (task_idx, project, _, cwd) in &rows {
            if task_key.contains_key(task_idx) {
                continue;
            }
            let display = project
                .clone()
                .unwrap_or_else(|| crate::store::fallback_project_identifier(cwd.as_deref()));
            task_key.insert(*task_idx, self.scope_project_key(cwd.as_deref(), &display));
        }

        let mut by_project: BTreeMap<String, Vec<SubprojectResolution>> = BTreeMap::new();
        for (task_idx, _, cell_idx, _) in &rows {
            let Some(key) = task_key.get(task_idx) else {
                continue;
            };
            let resolution = match resolved.get(&(*task_idx, *cell_idx)) {
                Some((subprojects, inferred)) => SubprojectResolution::Resolved {
                    subprojects: subprojects.clone(),
                    inferred: *inferred,
                },
                None => SubprojectResolution::Unresolved,
            };
            by_project.entry(key.clone()).or_default().push(resolution);
        }
        Ok(by_project
            .into_iter()
            .map(|(project, cells)| (project, aggregate_scope(&cells)))
            .collect())
    }

    /// A review's aggregate [`Scope`], per project -- the union of every
    /// squad's cells feeding into one of this guardian's branches (a
    /// manually-assembled review can pull cells from more than one squad,
    /// so this doesn't assume a single originating squad). Mirrors
    /// `Store::collecting_guardians_for_cells`'s cell/guardian join in the
    /// opposite direction: prefers each cell's direct `review_guardian_id`,
    /// falling back to a `review_branch` string match against
    /// `guardian_branches` for a cell with no direct linkage.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn review_scope_by_project(
        &self,
        guardian_id: &str,
    ) -> StoreResult<BTreeMap<String, Scope>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.squad_id, s.task_idx, s.idx FROM cells s
             WHERE s.review_guardian_id = ?
                OR (
                    s.review_guardian_id IS NULL
                    AND EXISTS (
                        SELECT 1 FROM guardian_branches gb
                        WHERE gb.guardian_id = ? AND gb.branch = s.review_branch
                    )
                )
             ORDER BY s.squad_id, s.task_idx, s.idx",
        )?;
        let rows = stmt
            .query_map(params![guardian_id, guardian_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut per_squad: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
        for (squad_id, task_idx, idx) in rows {
            per_squad.entry(squad_id).or_default().push((task_idx, idx));
        }

        let mut by_project: BTreeMap<String, Vec<SubprojectResolution>> = BTreeMap::new();
        for (squad_id, cells) in per_squad {
            let squad = self.get_squad(&squad_id)?;
            let resolved = Store::subprojects_by_cell(&self.conn, &squad_id)?;
            for (task_idx, idx) in cells {
                // Same registered-project keying as `squad_scope_by_project`:
                // both sides of the overlap test must agree on what a project
                // is, or a review and a squad in one repo could never match.
                let project = squad
                    .tasks
                    .get(usize::try_from(task_idx).unwrap_or(usize::MAX))
                    .map_or_else(
                        || "unassigned".to_string(),
                        |t| {
                            let cwd = t.cells.iter().find_map(|c| c.cwd.as_deref());
                            self.scope_project_key(cwd, &t.project)
                        },
                    );
                let resolution = match resolved.get(&(task_idx, idx)) {
                    Some((subprojects, inferred)) => SubprojectResolution::Resolved {
                        subprojects: subprojects.clone(),
                        inferred: *inferred,
                    },
                    None => SubprojectResolution::Unresolved,
                };
                by_project.entry(project).or_default().push(resolution);
            }
        }

        Ok(by_project
            .into_iter()
            .map(|(project, cells)| (project, aggregate_scope(&cells)))
            .collect())
    }

    /// A waypoint's aggregate [`Scope`], per project -- the union of every
    /// affected entry's own [`Store::squad_scope_by_project`] /
    /// [`Store::review_scope_by_project`], merged project-by-project via
    /// [`union_scope`]. Applies the same aggregation identically to review
    /// and squad affected entries, per Phase 0's design.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_scope_by_project(
        &self,
        waypoint_id: &str,
    ) -> StoreResult<BTreeMap<String, Scope>> {
        let mut merged: BTreeMap<String, Scope> = BTreeMap::new();
        for entry in self.list_affected_entries(waypoint_id)? {
            let entry_scope = match entry.kind {
                WaypointEntryKind::Squad => self.squad_scope_by_project(&entry.entry_id)?,
                WaypointEntryKind::Review => self.review_scope_by_project(&entry.entry_id)?,
            };
            for (project, scope) in entry_scope {
                merged
                    .entry(project)
                    .and_modify(|existing| *existing = union_scope(existing, &scope))
                    .or_insert(scope);
            }
        }
        Ok(merged)
    }

    /// The exact candidate set a waypoint's survey should invoke the LLM on
    /// (RAL-400 Phase 2): every open review / non-terminal squad that is not
    /// already an explicit affected entry, whose own [`Scope`] overlaps the
    /// waypoint's aggregate scope for a shared project (see
    /// [`scopes_overlap`]). Computed entirely from already-stored scope data
    /// -- no model call happens here -- so this is the "narrowest set"
    /// selection the survey itself then classifies.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_survey_candidates(
        &self,
        waypoint_id: &str,
    ) -> StoreResult<Vec<SurveyCandidate>> {
        let waypoint_scopes = self.waypoint_scope_by_project(waypoint_id)?;
        if waypoint_scopes.is_empty() {
            return Ok(Vec::new());
        }
        // A affecteded entry is normally not a candidate -- an explicit
        // declaration is never second-guessed by the classifier. The one
        // exception is an entry the *daemon* enrolled itself
        // (`auto_enrolled`) that hasn't been surveyed yet: submit-time
        // enrollment (`enroll_new_squad_in_open_waypoints`) deliberately
        // creates such rows to hold the gate closed until a verdict exists,
        // so they must stay surveyable or their NULL verdict would block
        // forever.
        let mut stmt = self.conn.prepare(
            "SELECT kind, entry_id FROM waypoint_affected
             WHERE waypoint_id=? AND NOT (auto_enrolled=1 AND survey_verdict IS NULL)",
        )?;
        let settled: Vec<(String, String)> = stmt
            .query_map(params![waypoint_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let already_on_affected: Vec<(WaypointEntryKind, String)> = settled
            .into_iter()
            .filter_map(|(kind, entry_id)| WaypointEntryKind::parse(&kind).map(|k| (k, entry_id)))
            .collect();
        let is_affecteded = |kind: WaypointEntryKind, id: &str| {
            already_on_affected
                .iter()
                .any(|(k, e)| *k == kind && e == id)
        };
        let overlaps = |cand_scopes: &BTreeMap<String, Scope>| {
            cand_scopes.iter().any(|(project, scope)| {
                waypoint_scopes
                    .get(project)
                    .is_some_and(|ws| scopes_overlap(ws, scope))
            })
        };

        let mut candidates = Vec::new();

        let mut stmt = self.conn.prepare("SELECT id, state FROM squads")?;
        let squad_rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for (squad_id, state) in squad_rows {
            let is_terminal = crate::store::SquadState::parse(&state)
                .is_some_and(crate::store::SquadState::is_terminal);
            if is_terminal || is_affecteded(WaypointEntryKind::Squad, &squad_id) {
                continue;
            }
            if overlaps(&self.squad_scope_by_project(&squad_id)?) {
                candidates.push(SurveyCandidate {
                    kind: WaypointEntryKind::Squad,
                    entry_id: squad_id,
                });
            }
        }

        for (guardian_id, status) in self.list_guardian_status_pairs()? {
            if GuardianStatus::is_terminal_status(&status)
                || is_affecteded(WaypointEntryKind::Review, &guardian_id)
            {
                continue;
            }
            if overlaps(&self.review_scope_by_project(&guardian_id)?) {
                candidates.push(SurveyCandidate {
                    kind: WaypointEntryKind::Review,
                    entry_id: guardian_id,
                });
            }
        }

        Ok(candidates)
    }

    /// Every currently-`open` waypoint's id.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_open_waypoint_ids(&self) -> StoreResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM waypoints WHERE state='open'")?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every currently-`closed` waypoint's id (RAL-400 Phase 6) -- feeds
    /// [`run_pending_stand_down_notices`], the closed-side counterpart to
    /// [`Store::list_open_waypoint_ids`].
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_closed_waypoint_ids(&self) -> StoreResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM waypoints WHERE state='closed'")?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether a waypoint is `open` (`true`) or `closed`/nonexistent
    /// (`false`).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_is_open(&self, id: &str) -> StoreResult<bool> {
        let state: Option<String> = self
            .conn
            .query_row("SELECT state FROM waypoints WHERE id=?", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(state.as_deref() == Some("open"))
    }

    /// Close a waypoint (idempotent -- closing an already-closed waypoint is
    /// a no-op). Returns `true` if this call actually transitioned it.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn close_waypoint(&self, id: &str) -> StoreResult<bool> {
        let now = now_ms();
        let n = self.conn.execute(
            "UPDATE waypoints SET state='closed', updated_at_ms=?, closed_at_ms=? WHERE id=? AND state != 'closed'",
            params![now, now, id],
        )?;
        Ok(n > 0)
    }

    /// Add (or update the mode of, if already present) one affected entry.
    /// Unique on `(waypoint_id, kind, entry_id)` -- re-adding the same entry
    /// updates its `mode` in place rather than duplicating the row.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn add_affected_entry(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
        mode: AffectedMode,
    ) -> StoreResult<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO waypoint_affected(waypoint_id, kind, entry_id, mode, delivery_status, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,'undelivered',?,?)
             ON CONFLICT(waypoint_id, kind, entry_id) DO UPDATE SET mode=excluded.mode, updated_at_ms=excluded.updated_at_ms",
            params![waypoint_id, kind.as_str(), entry_id, mode.as_str(), now, now],
        )?;
        Ok(())
    }

    /// [`Self::add_affected_entry`] for an entry the *daemon* enrolled rather
    /// than a human/agent declaring it -- submit-time scope overlap
    /// ([`Self::enroll_new_squad_in_open_waypoints`]) or the survey sweep's
    /// own discovery ([`survey_candidate`]).
    ///
    /// Identical to `add_affected_entry` except it marks the row
    /// `auto_enrolled`, which is what makes it eligible to be surveyed. An
    /// already-present row keeps whatever flag it has: a human's explicit
    /// declaration is never silently converted into a surveyable one, and an
    /// auto-enrolled row re-enrolled by a later sweep stays surveyable.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn enroll_affected_entry(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
        mode: AffectedMode,
    ) -> StoreResult<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO waypoint_affected(waypoint_id, kind, entry_id, mode, delivery_status, auto_enrolled, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,'undelivered',1,?,?)
             ON CONFLICT(waypoint_id, kind, entry_id) DO UPDATE SET mode=excluded.mode, updated_at_ms=excluded.updated_at_ms",
            params![waypoint_id, kind.as_str(), entry_id, mode.as_str(), now, now],
        )?;
        Ok(())
    }

    /// Enroll a just-submitted squad on every open waypoint whose scope
    /// overlaps it, as a blocking, not-yet-surveyed affected entry. Returns the
    /// waypoint ids it enrolled the squad on.
    ///
    /// This closes the submit-time gap the periodic survey sweep leaves open.
    /// The gate ([`Self::squad_block_gating_waypoint`]) is only consulted when
    /// a squad is *claimed*, and a survey-discovered affected entry doesn't
    /// exist until the next sweep -- up to
    /// `scheduler::WAYPOINT_SURVEY_INTERVAL` later. Without this, a squad
    /// submitted while a waypoint is open is dispatched immediately and
    /// unguarded; one whose work finishes inside that window escapes the
    /// waypoint entirely and then, being terminal, is permanently excluded
    /// from [`Self::waypoint_survey_candidates`] -- so the waypoint never
    /// learns it existed.
    ///
    /// Enrolling at `Block` with a NULL verdict is what makes this
    /// fail-closed without an LLM call in the submit path (which
    /// [`run_pending_surveys`]'s contract forbids): the gate already treats a
    /// NULL verdict as blocking, and the next sweep's survey either confirms
    /// the block or releases it with a `not_impacted` verdict.
    ///
    /// Uses the same project/scope overlap test as
    /// [`Self::waypoint_survey_candidates`], so a squad in an unrelated
    /// project -- or, in a monorepo, a confidently non-overlapping subproject
    /// -- is never enrolled and never delayed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn enroll_new_squad_in_open_waypoints(&self, squad_id: &str) -> StoreResult<Vec<String>> {
        let squad_scopes = self.squad_scope_by_project(squad_id)?;
        if squad_scopes.is_empty() {
            return Ok(Vec::new());
        }
        let mut enrolled = Vec::new();
        for waypoint_id in self.list_open_waypoint_ids()? {
            let waypoint_scopes = self.waypoint_scope_by_project(&waypoint_id)?;
            let overlaps = waypoint_scopes.iter().any(|(project, ws)| {
                squad_scopes
                    .get(project)
                    .is_some_and(|ss| scopes_overlap(ws, ss))
            });
            if !overlaps {
                continue;
            }
            let already = self
                .list_affected_entries(&waypoint_id)?
                .into_iter()
                .any(|e| e.kind == WaypointEntryKind::Squad && e.entry_id == squad_id);
            if already {
                continue;
            }
            self.enroll_affected_entry(
                &waypoint_id,
                WaypointEntryKind::Squad,
                squad_id,
                AffectedMode::Block,
            )?;
            enrolled.push(waypoint_id);
        }
        Ok(enrolled)
    }

    /// Remove one affected entry. Returns `true` if a row was actually
    /// removed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn remove_affected_entry(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
    ) -> StoreResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM waypoint_affected WHERE waypoint_id=? AND kind=? AND entry_id=?",
            params![waypoint_id, kind.as_str(), entry_id],
        )?;
        Ok(n > 0)
    }

    /// Every affected entry for a waypoint, oldest first.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_affected_entries(&self, waypoint_id: &str) -> StoreResult<Vec<AffectedEntryView>> {
        let mut stmt = self.conn.prepare(
            "SELECT waypoint_id, kind, entry_id, mode, survey_verdict, survey_rationale, delivery_status, stand_down_at_ms, created_at_ms, updated_at_ms, stale_at_ms, bearing_decision, bearing_decided_at_ms
             FROM waypoint_affected WHERE waypoint_id=? ORDER BY created_at_ms, entry_id",
        )?;
        let rows = stmt
            .query_map(params![waypoint_id], Self::map_affected_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn map_affected_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AffectedEntryView> {
        let kind_s: String = r.get(1)?;
        let mode_s: String = r.get(3)?;
        let delivery_s: String = r.get(6)?;
        Ok(AffectedEntryView {
            waypoint_id: r.get(0)?,
            kind: WaypointEntryKind::parse(&kind_s).unwrap_or(WaypointEntryKind::Squad),
            entry_id: r.get(2)?,
            mode: AffectedMode::parse(&mode_s).unwrap_or(AffectedMode::Block),
            survey_verdict: r.get(4)?,
            survey_rationale: r.get(5)?,
            delivery_status: DeliveryStatus::parse(&delivery_s)
                .unwrap_or(DeliveryStatus::Undelivered),
            stand_down_at_ms: r.get(7)?,
            created_at_ms: r.get(8)?,
            updated_at_ms: r.get(9)?,
            stale_at_ms: r.get(10)?,
            bearing_decision: r
                .get::<_, Option<String>>(11)?
                .as_deref()
                .and_then(BearingDecision::parse),
            bearing_decided_at_ms: r.get(12)?,
        })
    }

    /// Record a affected entry's delivery status transition.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_affected_delivery_status(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
        status: DeliveryStatus,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE waypoint_affected SET delivery_status=?, updated_at_ms=? WHERE waypoint_id=? AND kind=? AND entry_id=?",
            params![status.as_str(), now_ms(), waypoint_id, kind.as_str(), entry_id],
        )?;
        Ok(())
    }

    /// Mark a affected entry's advisory stand-down notice as sent (RAL-400
    /// Phase 6, idempotency for [`run_pending_stand_down_notices`]). A no-op
    /// if already marked -- `stand_down_at_ms` is set once and never
    /// overwritten, so calling this twice keeps the original timestamp.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn mark_affected_entry_stood_down(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE waypoint_affected SET stand_down_at_ms=?, updated_at_ms=?
             WHERE waypoint_id=? AND kind=? AND entry_id=? AND stand_down_at_ms IS NULL",
            params![now_ms(), now_ms(), waypoint_id, kind.as_str(), entry_id],
        )?;
        Ok(())
    }

    /// Update a waypoint's settings in place: label, guidance prompt, survey
    /// agent/model, and whether advisory entries are allowed.
    ///
    /// Never clears survey verdicts by itself. Editing guidance must not
    /// silently re-judge work on the strength of text the surveyed entries
    /// never saw -- re-deciding is a separate, explicit
    /// [`Self::clear_auto_enrolled_survey_verdicts`] call, which callers are
    /// expected to precede with [`Self::resurvey_preview`] so whoever asked
    /// for it can see what it will touch first.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if no such waypoint exists; otherwise
    /// propagates any SQLite failure.
    pub fn update_waypoint_settings(
        &self,
        id: &str,
        label: Option<&str>,
        prompt: &str,
        agent: Option<&str>,
        model: Option<&str>,
        allow_advisory: bool,
    ) -> StoreResult<()> {
        let n = self.conn.execute(
            "UPDATE waypoints SET label=?, prompt=?, agent=?, model=?, allow_advisory=?, updated_at_ms=?
             WHERE id=?",
            params![label, prompt, agent, model, allow_advisory, now_ms(), id],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// One affected entry's delivery status, or `None` if this waypoint does
    /// not affect that entry.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn affected_entry_delivery_status(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
    ) -> StoreResult<Option<DeliveryStatus>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT delivery_status FROM waypoint_affected
                 WHERE waypoint_id=? AND kind=? AND entry_id=?",
                params![waypoint_id, kind.as_str(), entry_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.as_deref().and_then(DeliveryStatus::parse))
    }

    /// Every open waypoint this squad is an affected entry of -- who a
    /// `RALPHUS_BEARING:` line from one of its cells is answering.
    ///
    /// A squad can be affected by more than one waypoint at a time, and the
    /// guidance for all of them arrives in the same prompt, so one answer is
    /// recorded against each. The marker carries no waypoint id to split them
    /// by, and asking an agent to repeat itself per waypoint would make the
    /// common case (exactly one) worse to serve the rare one.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn open_waypoints_affecting_squad(&self, squad_id: &str) -> StoreResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT wa.waypoint_id FROM waypoint_affected wa
             JOIN waypoints w ON w.id = wa.waypoint_id
             WHERE wa.kind = 'squad' AND wa.entry_id = ? AND w.state = 'open'
             ORDER BY wa.created_at_ms ASC",
        )?;
        let out: Vec<String> = stmt
            .query_map(params![squad_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// Every open waypoint this review is an affected entry of -- the review
    /// counterpart of [`Self::open_waypoints_affecting_squad`].
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn open_waypoints_affecting_review(&self, guardian_id: &str) -> StoreResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT wa.waypoint_id FROM waypoint_affected wa
             JOIN waypoints w ON w.id = wa.waypoint_id
             WHERE wa.kind = 'review' AND wa.entry_id = ? AND w.state = 'open'
             ORDER BY wa.created_at_ms ASC",
        )?;
        let out: Vec<String> = stmt
            .query_map(params![guardian_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// The most recent waypoint to have affected this squad, open or closed.
    ///
    /// A cell resuming from a hold has usually outlived the hold: the gate
    /// lifted because the waypoint's affected landed, and the waypoint may have
    /// closed on the same tick. Reporting "no waypoint" there would lose the
    /// one piece of context the resuming agent most needs.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn last_waypoint_affecting_squad(&self, squad_id: &str) -> StoreResult<Option<String>> {
        self.conn
            .query_row(
                "SELECT waypoint_id FROM waypoint_affected
                 WHERE kind = 'squad' AND entry_id = ?
                 ORDER BY created_at_ms DESC LIMIT 1",
                params![squad_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Tell everything this waypoint was holding that it is no longer held.
    ///
    /// Closing lifts every block-mode hold at once, and until this existed
    /// nothing said so: the entries were told when the hold went on
    /// ([`notify_entry_blocked`]) and then simply stopped being held, which
    /// reads from the outside like the first message was a dead end.
    ///
    /// Only entries that were actually being held are notified -- an advisory
    /// entry was never held, and a block-mode entry the survey had already
    /// cleared was already told.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn notify_affected_of_release(&self, waypoint_id: &str, reason: &str) -> StoreResult<()> {
        for entry in self.list_affected_entries(waypoint_id)? {
            let was_held = entry.mode == AffectedMode::Block
                && entry.survey_verdict.as_deref() != Some("not_impacted");
            if was_held {
                notify_entry_released(self, waypoint_id, entry.kind, &entry.entry_id, reason);
            }
        }
        Ok(())
    }

    /// Add a review or squad to the waypoint's completion list.
    ///
    /// Idempotent: adding something already listed updates its note rather
    /// than erroring, so a caller correcting a note does not have to remove
    /// and re-add.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn add_roster_entry(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
        note: Option<&str>,
    ) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO waypoint_roster(waypoint_id, kind, entry_id, note, created_at_ms)
             VALUES(?,?,?,?,?)
             ON CONFLICT(waypoint_id, kind, entry_id) DO UPDATE SET note=excluded.note",
            params![waypoint_id, kind.as_str(), entry_id, note, now_ms()],
        )?;
        Ok(())
    }

    /// Drop a goal from the waypoint's completion list. Removing the last
    /// unfinished goal can make the waypoint closeable, which is the point.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if that goal is not on the list;
    /// otherwise propagates any SQLite failure.
    pub fn remove_roster_entry(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
    ) -> StoreResult<()> {
        let n = self.conn.execute(
            "DELETE FROM waypoint_roster WHERE waypoint_id=? AND kind=? AND entry_id=?",
            params![waypoint_id, kind.as_str(), entry_id],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// The waypoint's completion list, each goal carrying whether it has
    /// finished yet.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_roster_entries(&self, waypoint_id: &str) -> StoreResult<Vec<RosterEntryView>> {
        let mut stmt = self.conn.prepare(
            "SELECT waypoint_id, kind, entry_id, note, created_at_ms
             FROM waypoint_roster WHERE waypoint_id=? ORDER BY created_at_ms ASC, entry_id ASC",
        )?;
        let raw: Vec<(String, String, String, Option<String>, i64)> = stmt
            .query_map(params![waypoint_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut out = Vec::with_capacity(raw.len());
        for (wp, kind_s, entry_id, note, created_at_ms) in raw {
            let Some(kind) = WaypointEntryKind::parse(&kind_s) else {
                continue;
            };
            out.push(RosterEntryView {
                waypoint_id: wp,
                kind,
                entry_id: entry_id.clone(),
                note,
                terminal: self.entry_work_is_terminal(kind, &entry_id)?,
                created_at_ms,
            });
        }
        Ok(out)
    }

    /// Whether a review/squad has reached a terminal state, by the same
    /// per-kind reading the rest of the waypoint module uses.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    fn entry_work_is_terminal(&self, kind: WaypointEntryKind, entry_id: &str) -> StoreResult<bool> {
        Ok(match kind {
            WaypointEntryKind::Squad => match self.squad_state(entry_id) {
                Ok(state) => state.is_terminal_for_waypoint(),
                Err(StoreError::NotFound) => false,
                Err(e) => return Err(e),
            },
            WaypointEntryKind::Review => {
                let status: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT status FROM guardians WHERE id=?",
                        params![entry_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                match status {
                    Some(s) => GuardianStatus::is_terminal_status(&s),
                    None => false,
                }
            }
        })
    }

    /// Record how one affected entry answered this waypoint.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if that entry is not affected by this
    /// waypoint; otherwise propagates any SQLite failure.
    pub fn set_affected_bearing_decision(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
        decision: BearingDecision,
    ) -> StoreResult<()> {
        let n = self.conn.execute(
            "UPDATE waypoint_affected SET bearing_decision=?, bearing_decided_at_ms=?, updated_at_ms=?
             WHERE waypoint_id=? AND kind=? AND entry_id=?",
            params![
                decision.as_str(),
                now_ms(),
                now_ms(),
                waypoint_id,
                kind.as_str(),
                entry_id
            ],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Whether every goal on the waypoint's completion list has finished --
    /// phase 1 of a waypoint being done.
    ///
    /// An empty list is vacuously satisfied. A waypoint that names no roster
    /// is a pure broadcast ("this is changing, tell me how it lands on you"),
    /// and the only thing left to wait for is the answers.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn roster_complete(&self, waypoint_id: &str) -> StoreResult<bool> {
        Ok(self
            .list_roster_entries(waypoint_id)?
            .iter()
            .all(|g| g.terminal))
    }

    /// Whether every affected entry that owes this waypoint an answer has
    /// given one -- phase 2 of a waypoint being done.
    ///
    /// Only `block`-mode entries owe an answer. An advisory entry is told
    /// what changed and left alone; waiting on one would make "advisory"
    /// mean nothing.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn affected_have_answered(&self, waypoint_id: &str) -> StoreResult<bool> {
        Ok(self
            .list_affected_entries(waypoint_id)?
            .iter()
            .filter(|e| e.mode == AffectedMode::Block)
            .all(|e| e.bearing_decision.is_some()))
    }

    /// What a re-survey of this waypoint would act on, so the caller can show
    /// it before committing to one.
    ///
    /// `targets` are the daemon-enrolled entries whose verdicts a re-survey
    /// clears, putting each back in the classifier's queue. `held_explicit`
    /// are the human-declared entries, listed only so the answer to "what
    /// does this touch" is complete -- an explicit declaration is never
    /// second-guessed by the classifier, so a re-survey leaves them alone.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if no such waypoint exists; otherwise
    /// propagates any SQLite failure.
    pub fn resurvey_preview(&self, waypoint_id: &str) -> StoreResult<ResurveyPreview> {
        // Proves the waypoint exists, so an unknown id is a 404 rather than
        // an empty preview that reads as "this would do nothing".
        let _ = self.get_waypoint(waypoint_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT kind, entry_id, mode, survey_verdict, delivery_status, auto_enrolled
             FROM waypoint_affected WHERE waypoint_id=? ORDER BY created_at_ms ASC, entry_id ASC",
        )?;
        let raw: Vec<(String, String, String, Option<String>, String, bool)> = stmt
            .query_map(params![waypoint_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let rows: Vec<(
            WaypointEntryKind,
            String,
            String,
            Option<String>,
            String,
            bool,
        )> = raw
            .into_iter()
            .filter_map(|(kind, entry_id, mode, verdict, delivery, auto)| {
                WaypointEntryKind::parse(&kind)
                    .map(|kind| (kind, entry_id, mode, verdict, delivery, auto))
            })
            .collect();

        let labels = self.affected_entry_labels(
            &rows
                .iter()
                .map(|(kind, entry_id, ..)| (*kind, entry_id.clone()))
                .collect::<Vec<_>>(),
        )?;
        let build = |(kind, entry_id, mode, verdict, delivery, _): &(
            WaypointEntryKind,
            String,
            String,
            Option<String>,
            String,
            bool,
        )| ResurveyTarget {
            kind: *kind,
            entry_id: entry_id.clone(),
            label: labels.get(&(*kind, entry_id.clone())).cloned().flatten(),
            mode: mode.clone(),
            current_verdict: verdict.clone(),
            delivery_status: delivery.clone(),
            // A block-mode entry whose verdict is cleared is held again until
            // the classifier re-judges it, because the gate reads a NULL
            // verdict as "not cleared". Worth saying out loud in a
            // confirmation: re-surveying a released entry can re-block it.
            will_be_held_until_judged: mode == "block",
        };
        let (auto, explicit): (Vec<_>, Vec<_>) = rows.iter().partition(|(.., auto)| *auto);
        Ok(ResurveyPreview {
            targets: auto.into_iter().map(build).collect(),
            held_explicit: explicit.into_iter().map(build).collect(),
        })
    }

    /// Display names for a batch of affected entries, as `(kind, entry_id) ->
    /// name`. One prepared statement per kind rather than one per entry; an
    /// entry whose row is gone (or which never had a name) maps to `None`
    /// rather than dropping out, so a caller can always account for every
    /// entry it asked about.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    fn affected_entry_labels(
        &self,
        entries: &[(WaypointEntryKind, String)],
    ) -> StoreResult<std::collections::HashMap<(WaypointEntryKind, String), Option<String>>> {
        let mut out: std::collections::HashMap<(WaypointEntryKind, String), Option<String>> =
            entries
                .iter()
                .map(|(k, id)| ((*k, id.clone()), None))
                .collect();
        for (kind, sql) in [
            (
                WaypointEntryKind::Squad,
                "SELECT label FROM squads WHERE id = ?",
            ),
            (
                WaypointEntryKind::Review,
                "SELECT name FROM guardians WHERE id = ?",
            ),
        ] {
            let ids: Vec<&String> = entries
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, id)| id)
                .collect();
            if ids.is_empty() {
                continue;
            }
            let mut stmt = self.conn.prepare(sql)?;
            for id in ids {
                let name: Option<String> = stmt
                    .query_row(params![id], |r| r.get::<_, Option<String>>(0))
                    .optional()?
                    .flatten();
                out.insert((kind, id.clone()), name.filter(|n| !n.trim().is_empty()));
            }
        }
        Ok(out)
    }

    /// Put every daemon-enrolled entry back in the classifier's queue by
    /// clearing its verdict, so the next sweep re-judges it against the
    /// waypoint's current guidance. Returns how many rows were cleared.
    ///
    /// Human-declared (`auto_enrolled = 0`) entries are left alone: the
    /// classifier does not second-guess an explicit declaration, so clearing
    /// their verdict would strand them -- never re-judged, and now reading as
    /// unsurveyed to the gate, which fails closed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn clear_auto_enrolled_survey_verdicts(&self, waypoint_id: &str) -> StoreResult<usize> {
        let n = self.conn.execute(
            "UPDATE waypoint_affected
             SET survey_verdict=NULL, survey_rationale=NULL, updated_at_ms=?
             WHERE waypoint_id=? AND auto_enrolled=1",
            params![now_ms(), waypoint_id],
        )?;
        Ok(n)
    }

    /// Delivery-status counts for every waypoint at once, keyed by waypoint id.
    ///
    /// The list endpoint renders a progress meter per row, which previously
    /// meant loading each waypoint's full affected -- every entry's verdict,
    /// rationale and timestamps -- just to tally four numbers off it, once per
    /// waypoint. This counts in SQL in a single pass instead, so listing N
    /// waypoints is one query rather than N.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_delivery_counts(&self) -> StoreResult<BTreeMap<String, BTreeMap<String, i64>>> {
        let mut stmt = self.conn.prepare(
            "SELECT waypoint_id, delivery_status, COUNT(*) FROM waypoint_affected
             GROUP BY waypoint_id, delivery_status",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        for (waypoint_id, status, n) in rows {
            out.entry(waypoint_id).or_default().insert(status, n);
        }
        Ok(out)
    }

    /// Every cell of `squad_id` this waypoint actually advised -- i.e. that
    /// had guidance queued for it, delivered or not.
    ///
    /// This is the precise audience for the waypoint's stand-down notice: the
    /// cells that were told something are the ones for whom "that guidance is
    /// now moot" is news. Returns `(task_idx, idx)` pairs, oldest first.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_advised_cells(
        &self,
        waypoint_id: &str,
        squad_id: &str,
    ) -> StoreResult<Vec<(i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT target_task, target_idx FROM pending_injections
             WHERE waypoint_id=? AND target_squad=?
             ORDER BY target_task, target_idx",
        )?;
        let rows = stmt
            .query_map(params![waypoint_id, squad_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Queue one newly-appended bearing for delivery to every **advisory**
    /// squad affected entry on this waypoint, one injection per not-yet-finished
    /// cell. Returns how many injections were queued.
    ///
    /// This is the advisory counterpart to the blocking path's ghost-fold. A
    /// `block`-mode entry gets its bearings when its cell is halted and
    /// re-dispatched; an `advisory` entry is deliberately never halted, so
    /// nothing was reaching it at all -- `pending_injections` existed and was
    /// unit-tested but had no production writer. This is that writer; the
    /// scheduler's cell-dispatch drain is the reader.
    ///
    /// All injections from one bearing share a `batch_id` (the bearing's own
    /// rowid), so a superseded bearing can be withdrawn wholesale via
    /// [`Self::cancel_injection_batch`] without tracking individual rows.
    ///
    /// Scope note: a cell that is *already running* only sees this on its next
    /// dispatch -- there is no mid-turn injection into a live agent process,
    /// which needs per-backend session semantics and stays out. In practice
    /// that means the squad's later cells receive it, and a running cell
    /// receives it if it is restarted or resumed.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn queue_advisory_bearing_injections(
        &self,
        waypoint_id: &str,
        bearing: &BearingView,
    ) -> StoreResult<usize> {
        let payload = render_bearing_block(waypoint_id, std::slice::from_ref(bearing))
            .unwrap_or_else(|| bearing.summary.clone());
        let batch_id = format!("bearing-{}", bearing.id);
        let mut queued = 0usize;
        for entry in self.list_affected_entries(waypoint_id)? {
            if entry.mode != AffectedMode::Advisory || entry.kind != WaypointEntryKind::Squad {
                continue;
            }
            let mut stmt = self.conn.prepare(
                "SELECT task_idx, idx FROM cells
                 WHERE squad_id=? AND state NOT IN ('done','failed','cancelled','ignored')
                 ORDER BY task_idx, idx",
            )?;
            let targets: Vec<(i64, i64)> = stmt
                .query_map(params![entry.entry_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(stmt);
            for (task_idx, idx) in targets {
                self.enqueue_injection(
                    &entry.entry_id,
                    task_idx,
                    idx,
                    &payload,
                    Some(&batch_id),
                    Some(waypoint_id),
                )?;
                queued += 1;
            }
        }
        Ok(queued)
    }

    /// Flag a affected entry's already-finished work as possibly needing a
    /// redo. Idempotent, and set-once like
    /// [`Self::mark_affected_entry_stood_down`] -- a reopen+reclose cycle never
    /// re-flags an entry whose flag a redo already cleared, so the notice
    /// isn't re-sent for work someone has already dealt with.
    ///
    /// Returns `true` if this call is what set the flag.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn mark_affected_entry_stale(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
    ) -> StoreResult<bool> {
        let now = now_ms();
        let n = self.conn.execute(
            "UPDATE waypoint_affected SET stale_at_ms=?, updated_at_ms=?
             WHERE waypoint_id=? AND kind=? AND entry_id=? AND stale_at_ms IS NULL",
            params![now, now, waypoint_id, kind.as_str(), entry_id],
        )?;
        Ok(n > 0)
    }

    /// Clear a affected entry's stale flag, once its redo has been kicked off
    /// (or the flag dismissed). Returns `true` if a flag was actually
    /// cleared.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn clear_affected_entry_stale(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
    ) -> StoreResult<bool> {
        let n = self.conn.execute(
            "UPDATE waypoint_affected SET stale_at_ms=NULL, updated_at_ms=?
             WHERE waypoint_id=? AND kind=? AND entry_id=? AND stale_at_ms IS NOT NULL",
            params![now_ms(), waypoint_id, kind.as_str(), entry_id],
        )?;
        Ok(n > 0)
    }

    /// Whether this affected entry's work has reached a terminal state -- the
    /// same per-kind terminality [`Self::all_affected_entries_terminal`] uses,
    /// for one entry instead of the whole affected.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn affected_entry_work_is_terminal(
        &self,
        kind: WaypointEntryKind,
        entry_id: &str,
    ) -> StoreResult<bool> {
        match kind {
            WaypointEntryKind::Squad => match self.squad_state(entry_id) {
                Ok(state) => Ok(state.is_terminal_for_waypoint()),
                Err(StoreError::NotFound) => Ok(false),
                Err(e) => Err(e),
            },
            WaypointEntryKind::Review => {
                let status: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT status FROM guardians WHERE id=?",
                        params![entry_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok(status.is_some_and(|s| GuardianStatus::is_terminal_status(&s)))
            }
        }
    }

    /// Record one candidate's survey outcome (Phase 2): sets `mode` and the
    /// `survey_verdict`/`survey_rationale` columns. Does not itself add the
    /// affected entry -- callers add it (or update its `mode` in place, since
    /// [`Store::add_affected_entry`] is an upsert) before calling this.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_affected_survey_result(
        &self,
        waypoint_id: &str,
        kind: WaypointEntryKind,
        entry_id: &str,
        verdict: &SurveyVerdict,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE waypoint_affected SET mode=?, survey_verdict=?, survey_rationale=?, updated_at_ms=?
             WHERE waypoint_id=? AND kind=? AND entry_id=?",
            params![
                verdict.mode.as_str(),
                if verdict.impacted { "impacted" } else { "not_impacted" },
                verdict.rationale,
                now_ms(),
                waypoint_id,
                kind.as_str(),
                entry_id
            ],
        )?;
        Ok(())
    }

    /// Whether every affected entry on a waypoint has reached a terminal state
    /// (squad terminal states, per [`SquadState::is_terminal_for_waypoint`]
    /// -- deliberately stricter than the generic [`SquadState::is_terminal`],
    /// since a `failed` squad can still be restarted back to `pending` via
    /// [`Store::restart_squad`] and so is not "finished" for this purpose --
    /// count the same as review terminal states, per
    /// [`GuardianStatus::is_terminal_status`], which already excludes
    /// `merge_failed` for the same reason). A waypoint with an empty
    /// affected is never considered terminal -- there is nothing to have
    /// finished yet.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn all_affected_entries_terminal(&self, waypoint_id: &str) -> StoreResult<bool> {
        let entries = self.list_affected_entries(waypoint_id)?;
        if entries.is_empty() {
            return Ok(false);
        }
        for entry in &entries {
            let terminal = match entry.kind {
                WaypointEntryKind::Squad => match self.squad_state(&entry.entry_id) {
                    Ok(state) => state.is_terminal_for_waypoint(),
                    Err(StoreError::NotFound) => false,
                    Err(e) => return Err(e),
                },
                WaypointEntryKind::Review => {
                    let status: Option<String> = self
                        .conn
                        .query_row(
                            "SELECT status FROM guardians WHERE id=?",
                            params![entry.entry_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    match status {
                        Some(s) => GuardianStatus::is_terminal_status(&s),
                        None => false,
                    }
                }
            };
            if !terminal {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Close a waypoint if every affected entry has reached a terminal state
    /// (RAL-400 Phase 6). A no-op (returns `false`) if the waypoint is
    /// already closed, has no affected entries, or has at least one
    /// still-active entry. Records a Cartographer row on an actual close.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn maybe_auto_close_waypoint(&self, waypoint_id: &str) -> StoreResult<bool> {
        if !self.waypoint_is_open(waypoint_id)? {
            return Ok(false);
        }
        // A waypoint that names nothing and affects nothing has not finished,
        // it has not started -- closing one the moment it is created would be
        // the only thing this ever did for it.
        if self.list_roster_entries(waypoint_id)?.is_empty()
            && self.list_affected_entries(waypoint_id)?.is_empty()
        {
            return Ok(false);
        }
        // Phase 1: the work this waypoint consists of has landed.
        if !self.roster_complete(waypoint_id)? {
            return Ok(false);
        }
        // Phase 2: everything it lands on has said how it landed. Note the
        // order -- downstream work cannot honestly answer "did you take this
        // up" until the change it is answering about actually exists.
        if !self.affected_have_answered(waypoint_id)? {
            return Ok(false);
        }
        if !self.all_affected_entries_terminal(waypoint_id)? {
            return Ok(false);
        }
        let closed = self.close_waypoint(waypoint_id)?;
        if closed {
            log_swallowed(
                self,
                &format!("waypoint {waypoint_id} cancel queued injections on auto-close"),
                self.cancel_queued_injections_for_waypoint(waypoint_id),
            );
            log_swallowed(
                self,
                &format!("waypoint {waypoint_id} release notice on auto-close"),
                self.notify_affected_of_release(waypoint_id, "the waypoint closed"),
            );
            crate::cartographer::Note::new("waypoints")
                .scope("waypoint")
                .emit(
                    self,
                    format!(
                        "waypoint {waypoint_id} auto-closed: its affected landed and every blocking affected entry answered"
                    ),
                    serde_json::json!({"waypoint_id": waypoint_id, "reason": "auto"}),
                );
        }
        Ok(closed)
    }

    /// Every open waypoint that lists `(kind, entry_id)` as affected -- the reverse
    /// lookup behind the auto-close hooks in [`Store::set_squad_state`] and
    /// [`Store::set_guardian_status`], used to find which waypoints might now
    /// be closeable after one of their affected entries just reached a
    /// terminal state. Two waypoints may independently affected the same
    /// review/squad, so this can return more than one id.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoints_referencing_affected_entry(
        &self,
        kind: WaypointEntryKind,
        entry_id: &str,
    ) -> StoreResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT wr.waypoint_id FROM waypoint_affected wr
             JOIN waypoints w ON w.id = wr.waypoint_id
             WHERE wr.kind = ? AND wr.entry_id = ? AND w.state = 'open'",
        )?;
        let rows = stmt
            .query_map(params![kind.as_str(), entry_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Try to auto-close every open waypoint that lists `(kind, entry_id)` as affected
    /// (RAL-400 Phase 6) -- called from [`Store::set_squad_state`] and
    /// [`Store::set_guardian_status`] right after a affected entry's owning
    /// squad/review transitions into a terminal state.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn maybe_auto_close_waypoints_for_affected_entry(
        &self,
        kind: WaypointEntryKind,
        entry_id: &str,
    ) -> StoreResult<()> {
        for waypoint_id in self.waypoints_referencing_affected_entry(kind, entry_id)? {
            self.maybe_auto_close_waypoint(&waypoint_id)?;
        }
        Ok(())
    }

    /// Manually close a waypoint (RAL-400 Phase 6). Unlike
    /// [`Store::maybe_auto_close_waypoint`], this always closes an open
    /// waypoint regardless of affected state -- the whole point of a manual
    /// close is to override auto-close, e.g. to gate-release a squad whose
    /// review is still pending. Idempotent: closing an already-closed
    /// waypoint is a no-op. Takes effect for gating immediately, since
    /// [`Store::squad_block_gating_waypoint`] filters on live `state='open'`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn close_waypoint_manually(&self, id: &str) -> StoreResult<bool> {
        let closed = self.close_waypoint(id)?;
        if closed {
            log_swallowed(
                self,
                &format!("waypoint {id} cancel queued injections on manual close"),
                self.cancel_queued_injections_for_waypoint(id),
            );
            log_swallowed(
                self,
                &format!("waypoint {id} release notice on manual close"),
                self.notify_affected_of_release(id, "the waypoint was closed by hand"),
            );
            crate::cartographer::Note::new("waypoints")
                .scope("waypoint")
                .emit(
                    self,
                    format!("waypoint {id} closed manually"),
                    serde_json::json!({"waypoint_id": id, "reason": "manual"}),
                );
        }
        Ok(closed)
    }

    /// Reopen a closed waypoint (RAL-400 Phase 6): flips it back to `open`
    /// and clears `closed_at_ms`. Idempotent: reopening an already-open
    /// waypoint is a no-op. Deliberately does not reset any affected entry's
    /// `stand_down_at_ms` -- a later re-close must not re-send a stand-down
    /// notice that already went out.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn reopen_waypoint(&self, id: &str) -> StoreResult<bool> {
        let now = now_ms();
        let n = self.conn.execute(
            "UPDATE waypoints SET state='open', updated_at_ms=?, closed_at_ms=NULL WHERE id=? AND state != 'open'",
            params![now, id],
        )?;
        let reopened = n > 0;
        if reopened {
            crate::cartographer::Note::new("waypoints")
                .scope("waypoint")
                .emit(
                    self,
                    format!("waypoint {id} reopened"),
                    serde_json::json!({"waypoint_id": id}),
                );
        }
        Ok(reopened)
    }

    /// The open waypoint (if any) that block-gates a squad (RAL-400 Phase 3,
    /// scenario 1): a `kind='squad'` affected entry for `squad_id` whose `mode`
    /// is `block` and whose owning waypoint is still `open`. A affected entry
    /// with `survey_verdict='not_impacted'` never gates regardless of `mode`
    /// (the survey found this squad isn't actually affected, so the `mode`
    /// column's leftover default value is moot -- see
    /// [`resolve_survey_verdict`]/[`parse_survey_reply`]); a `NULL`
    /// `survey_verdict` (not yet surveyed, or a manually-added entry) gates,
    /// matching Phase 0's fail-closed rule and giving Phase 3's "gate the
    /// squad until classification completes" its effect for free, since
    /// [`survey_candidate`] already writes the `block`-mode affected row before
    /// its LLM call resolves. Advisory-mode entries never gate (Phase 0: for
    /// a squad, advisory means "keep running, just inform").
    ///
    /// Consulted by [`Store::list_ready`] and the queue view's `blocked_by`
    /// construction -- both existing call sites, not a new scheduling path.
    /// Returns the earliest-created blocking waypoint's id when more than one
    /// applies.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn squad_block_gating_waypoint(&self, squad_id: &str) -> StoreResult<Option<String>> {
        self.first_gating_waypoint(squad_id)
    }

    /// Every squad currently held by an open `block`-mode waypoint, in two
    /// queries -- the set-based form of calling
    /// [`Self::squad_block_gating_waypoint`] once per squad.
    ///
    /// The first query reads the `(squad, waypoint)` candidate pairs, the
    /// second the roster of just those waypoints with each roster entry's
    /// squad state / review status joined in. Which of those states count as
    /// terminal is still decided in Rust, by [`gated_squads_in`].
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub(crate) fn block_gated_squads(&self) -> StoreResult<BTreeSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT wr.entry_id, wr.waypoint_id FROM waypoint_affected wr
             JOIN waypoints w ON w.id = wr.waypoint_id
             WHERE wr.kind = 'squad' AND wr.mode = 'block'
               AND (wr.survey_verdict IS NULL OR wr.survey_verdict = 'impacted')
               AND w.state = 'open'",
        )?;
        let candidates: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        if candidates.is_empty() {
            return Ok(BTreeSet::new());
        }

        let mut stmt = self.conn.prepare(
            "SELECT r.waypoint_id, r.kind, s.state, g.status FROM waypoint_roster r
             LEFT JOIN squads s ON r.kind = 'squad' AND s.id = r.entry_id
             LEFT JOIN guardians g ON r.kind = 'review' AND g.id = r.entry_id
             WHERE r.waypoint_id IN (
                 SELECT wr.waypoint_id FROM waypoint_affected wr
                 JOIN waypoints w ON w.id = wr.waypoint_id
                 WHERE wr.kind = 'squad' AND wr.mode = 'block'
                   AND (wr.survey_verdict IS NULL OR wr.survey_verdict = 'impacted')
                   AND w.state = 'open')",
        )?;
        let roster: Vec<RosterRow> = stmt
            .query_map([], |r| {
                Ok(RosterRow {
                    waypoint_id: r.get(0)?,
                    kind: r.get(1)?,
                    squad_state: r.get(2)?,
                    review_status: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(gated_squads_in(&candidates, &roster))
    }

    /// The first open waypoint holding this squad back from being scheduled.
    ///
    /// A hold lasts only while the waypoint's own affected is still unfinished.
    /// That is the whole shape of the thing: affected work is held because the
    /// change it must take up does not exist yet, and once the affected lands
    /// there is nothing left to wait for -- the entry is released precisely so
    /// it can do the work and answer. Holding it through phase 2 as well would
    /// deadlock, since phase 2 is waiting on that answer.
    ///
    /// The affected-complete half is decided in Rust rather than folded into the
    /// query: which squad and review states count as terminal is already
    /// stated once, in [`SquadState::is_terminal_for_waypoint`] and
    /// [`GuardianStatus::is_terminal_status`], and restating those literals in
    /// SQL is how the two quietly stop agreeing.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    fn first_gating_waypoint(&self, entry_id: &str) -> StoreResult<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT wr.waypoint_id FROM waypoint_affected wr
             JOIN waypoints w ON w.id = wr.waypoint_id
             WHERE wr.kind = 'squad' AND wr.entry_id = ? AND wr.mode = 'block'
               AND (wr.survey_verdict IS NULL OR wr.survey_verdict = 'impacted')
               AND w.state = 'open'
             ORDER BY wr.created_at_ms ASC",
        )?;
        let candidates: Vec<String> = stmt
            .query_map(params![entry_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for waypoint_id in candidates {
            if !self.roster_complete(&waypoint_id)? {
                return Ok(Some(waypoint_id));
            }
        }
        Ok(None)
    }

    /// The first open waypoint holding this *review*, if any -- the review
    /// counterpart of [`Self::squad_block_gating_waypoint`], with the same
    /// fail-closed verdict reading (a NULL verdict still blocks, since an
    /// unsurveyed entry is not a cleared one).
    ///
    /// Phase 0 defines block mode for a review as "hold approval until the
    /// waypoint closes", but nothing consulted that: a `block`-mode review
    /// entry received its feedback and was then free to be approved and
    /// merged anyway, which is exactly the silent-bypass the ticket's Risks
    /// section warns about. [`Store::approve_guardian`] now checks this.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn review_block_gating_waypoint(&self, guardian_id: &str) -> StoreResult<Option<String>> {
        // A review's hold is on its *approval*, not on its work, so unlike a
        // squad it can be held right up until it answers: it stays able to
        // run, resolve and report a bearing the whole time, and only merging
        // is withheld. That makes "has it answered yet" a usable release
        // condition here, where for a squad -- whose hold is on being
        // scheduled at all -- it would be circular.
        let mut stmt = self.conn.prepare(
            "SELECT wr.waypoint_id, wr.bearing_decision FROM waypoint_affected wr
             JOIN waypoints w ON w.id = wr.waypoint_id
             WHERE wr.kind = 'review' AND wr.entry_id = ? AND wr.mode = 'block'
               AND (wr.survey_verdict IS NULL OR wr.survey_verdict = 'impacted')
               AND w.state = 'open'
             ORDER BY wr.created_at_ms ASC",
        )?;
        let candidates: Vec<(String, Option<String>)> = stmt
            .query_map(params![guardian_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for (waypoint_id, decision) in candidates {
            if decision.is_none() || !self.roster_complete(&waypoint_id)? {
                return Ok(Some(waypoint_id));
            }
        }
        Ok(None)
    }

    /// Every cell currently halted because its squad-kind affected entry
    /// became `mode=block` on an open waypoint (RAL-400 Phase 3, see
    /// [`Store::mark_cell_waypoint_halted`]) -- `(squad_id, task_idx, idx)`
    /// triples, oldest halt first. Consumed by [`run_pending_waypoint_resumes`]
    /// to find cells worth re-checking against
    /// [`Store::squad_block_gating_waypoint`] once a waypoint closes or
    /// de-escalates.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_halted_cells(&self) -> StoreResult<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT squad_id, task_idx, idx FROM cells
             WHERE waypoint_halted_at_ms IS NOT NULL
             ORDER BY waypoint_halted_at_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// RAL-400 Phase 3: once a squad that carries a `kind='squad'` affected
    /// entry completes and its review forms (`guardian_id` becomes known),
    /// retire that entry and add an equivalent `kind='review'` entry in its
    /// place -- same waypoint, mode, and survey verdict/rationale carried
    /// over -- so gating/delivery (Phase 4) continues through the review
    /// instead of the now-stale squad entry. A no-op if `squad_id` has no
    /// squad-kind affected entry on any waypoint, and idempotent if called more
    /// than once for the same `(squad_id, guardian_id)` pair (the review-kind
    /// insert is the same `ON CONFLICT` upsert [`Store::add_affected_entry`]
    /// uses elsewhere, keyed on `(waypoint_id, kind, entry_id)`).
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn transition_squad_affected_entries_to_review(
        &self,
        squad_id: &str,
        guardian_id: &str,
    ) -> StoreResult<()> {
        let mut stmt = self.conn.prepare(
            "SELECT waypoint_id, mode, survey_verdict, survey_rationale, bearing_decision, bearing_decided_at_ms
             FROM waypoint_affected WHERE kind='squad' AND entry_id=?",
        )?;
        let rows = stmt
            .query_map(params![squad_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let now = now_ms();
        for (
            waypoint_id,
            mode,
            survey_verdict,
            survey_rationale,
            bearing_decision,
            bearing_decided_at_ms,
        ) in rows
        {
            // The bearing decision travels with the entry: it is the same
            // work answering the same waypoint, and an entry that was already
            // answered (a follow-up squad's waypoint answers for it up front)
            // must not turn back into an unanswered block when its review
            // forms.
            self.conn.execute(
                "INSERT INTO waypoint_affected(waypoint_id, kind, entry_id, mode, survey_verdict, survey_rationale, delivery_status, created_at_ms, updated_at_ms, bearing_decision, bearing_decided_at_ms)
                 VALUES(?,'review',?,?,?,?,'undelivered',?,?,?,?)
                 ON CONFLICT(waypoint_id, kind, entry_id) DO UPDATE SET
                     mode=excluded.mode,
                     survey_verdict=excluded.survey_verdict,
                     survey_rationale=excluded.survey_rationale,
                     bearing_decision=COALESCE(excluded.bearing_decision, bearing_decision),
                     bearing_decided_at_ms=COALESCE(excluded.bearing_decided_at_ms, bearing_decided_at_ms),
                     updated_at_ms=excluded.updated_at_ms",
                params![
                    waypoint_id,
                    guardian_id,
                    mode,
                    survey_verdict,
                    survey_rationale,
                    now,
                    now,
                    bearing_decision,
                    bearing_decided_at_ms
                ],
            )?;
            self.conn.execute(
                "DELETE FROM waypoint_affected WHERE waypoint_id=? AND kind='squad' AND entry_id=?",
                params![waypoint_id, squad_id],
            )?;
        }
        Ok(())
    }

    /// Append one bearing to a waypoint's guidance history. Append-only --
    /// there is no corresponding update/delete method.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    #[allow(clippy::too_many_arguments)]
    pub fn append_waypoint_bearing(
        &self,
        waypoint_id: &str,
        producer_kind: WaypointEntryKind,
        producer_id: &str,
        summary: &str,
        entity_uri: Option<&str>,
        commit_id: Option<&str>,
        commit_summary: Option<&str>,
    ) -> StoreResult<BearingView> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO waypoint_bearings(waypoint_id, producer_kind, producer_id, summary, entity_uri, commit_id, commit_summary, created_at_ms)
             VALUES(?,?,?,?,?,?,?,?)",
            params![
                waypoint_id,
                producer_kind.as_str(),
                producer_id,
                summary,
                entity_uri,
                commit_id,
                commit_summary,
                now
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(BearingView {
            id,
            waypoint_id: waypoint_id.to_string(),
            producer_kind,
            producer_id: producer_id.to_string(),
            summary: summary.to_string(),
            entity_uri: entity_uri.map(str::to_string),
            commit_id: commit_id.map(str::to_string),
            commit_summary: commit_summary.map(str::to_string),
            created_at_ms: now,
        })
    }

    /// Every bearing recorded for one waypoint, in arrival order. Scoped
    /// strictly to `waypoint_id` -- never returns another waypoint's rows.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_waypoint_bearings(&self, waypoint_id: &str) -> StoreResult<Vec<BearingView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, waypoint_id, producer_kind, producer_id, summary, entity_uri, commit_id, commit_summary, created_at_ms
             FROM waypoint_bearings WHERE waypoint_id=? ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![waypoint_id], |r| {
                let kind_s: String = r.get(2)?;
                Ok(BearingView {
                    id: r.get(0)?,
                    waypoint_id: r.get(1)?,
                    producer_kind: WaypointEntryKind::parse(&kind_s)
                        .unwrap_or(WaypointEntryKind::Squad),
                    producer_id: r.get(3)?,
                    summary: r.get(4)?,
                    entity_uri: r.get(5)?,
                    commit_id: r.get(6)?,
                    commit_summary: r.get(7)?,
                    created_at_ms: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// This waypoint's merged delivery/event history (RAL-400 Phase 7): every
    /// Cartographer row this module has emitted for `waypoint_id`
    /// (creation, affected changes, manual close/reopen, auto-close, bearing
    /// appends, ...), oldest first. Backs `GET /api/waypoints/{id}/deliveries`,
    /// the waypoint analog of [`crate::timeline::build_squad_timeline`].
    ///
    /// The underlying Cartographer query can only filter by `source`/`scope`,
    /// not by a specific waypoint id, so this pages through every
    /// `source="waypoints"` row and keeps the ones whose `payload.waypoint_id`
    /// matches -- bounded by `WAYPOINT_MAX_SCANNED_EVENTS` so a store with a
    /// long waypoint history can't turn this into an unbounded scan.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn waypoint_deliveries(&self, waypoint_id: &str) -> StoreResult<Vec<WaypointEventEntry>> {
        // Attribution is matched in SQL, not in Rust. This used to page the
        // whole `scope='waypoint'` slice into memory -- full rows, up to
        // `WAYPOINT_MAX_SCANNED_EVENTS` of them, with `cartographer_query`
        // running its own `COUNT(*)` per page on top -- and then keep the
        // handful whose payload named this waypoint. That is fetching a large
        // object to pull a small piece out of it, and the cost grew with every
        // row the daemon had ever logged rather than with this waypoint's own
        // history.
        //
        // The `waypoint_ids` arm is the array shape one effect spanning
        // several waypoints uses; `json_each`
        // expands it so both shapes match in one pass.
        //
        // Only the columns `WaypointEventEntry` actually carries are selected:
        // `id`, `log_path` and `admin_only` are never rendered.
        let mut stmt = self.conn.prepare(
            "SELECT at_ms, level, source, message, squad_id, guardian_id, cell_id, task, payload
             FROM cartographer_events
             WHERE scope = 'waypoint'
               AND (json_extract(payload, '$.waypoint_id') = ?1
                    OR EXISTS (SELECT 1 FROM json_each(payload, '$.waypoint_ids')
                               WHERE json_each.value = ?1))
             ORDER BY at_ms ASC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![waypoint_id, WAYPOINT_MAX_EVENTS], |r| {
                let payload: String = r.get(8)?;
                Ok(WaypointEventEntry {
                    at_ms: r.get(0)?,
                    level: r.get(1)?,
                    source: r.get(2)?,
                    message: r.get(3)?,
                    squad_id: r.get(4)?,
                    guardian_id: r.get(5)?,
                    cell_id: r.get(6)?,
                    task: r.get(7)?,
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Queue an injection payload for one cell, optionally as part of a
    /// batch, attributed to the waypoint whose guidance it carries.
    ///
    /// `waypoint_id` is what lets the delivery show up in that waypoint's own
    /// event feed once drained; without it a delivered injection is invisible
    /// to any per-waypoint view.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn enqueue_injection(
        &self,
        target_squad: &str,
        target_task: i64,
        target_idx: i64,
        payload: &str,
        batch_id: Option<&str>,
        waypoint_id: Option<&str>,
    ) -> StoreResult<i64> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO pending_injections(target_squad, target_task, target_idx, payload, status, batch_id, waypoint_id, created_at_ms, updated_at_ms)
             VALUES(?,?,?,?,'queued',?,?,?,?)",
            params![target_squad, target_task, target_idx, payload, batch_id, waypoint_id, now, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Cancel every still-queued injection in one bearing's batch.
    ///
    /// The per-bearing counterpart of
    /// [`Self::cancel_queued_injections_for_waypoint`]. Nothing calls it yet:
    /// withdrawing a single bearing needs a bearing edit/delete surface, and
    /// bearings are deliberately append-only in v1.
    ///
    /// Cancel wins over a
    /// concurrent drain: both use the same `status='queued'` guard, so
    /// whichever transaction commits first determines each row's outcome,
    /// and a row can never be both delivered and cancelled. Returns the
    /// number of rows cancelled.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn cancel_injection_batch(&self, batch_id: &str) -> StoreResult<usize> {
        let n = self.conn.execute(
            "UPDATE pending_injections SET status='cancelled', updated_at_ms=? WHERE batch_id=? AND status='queued'",
            params![now_ms(), batch_id],
        )?;
        Ok(n)
    }

    /// Cancel every still-queued injection belonging to one waypoint.
    ///
    /// Called when a waypoint closes. An injection is only delivered when its
    /// target cell is next dispatched, so one queued against a cell that has
    /// not run yet would otherwise arrive long after the waypoint it speaks
    /// for is closed -- telling an agent to coordinate around something that
    /// finished. Same `status='queued'` guard as the drain, so a row can
    /// never be both delivered and cancelled.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn cancel_queued_injections_for_waypoint(&self, waypoint_id: &str) -> StoreResult<usize> {
        let n = self.conn.execute(
            "UPDATE pending_injections SET status='cancelled', updated_at_ms=?
             WHERE waypoint_id=? AND status='queued'",
            params![now_ms(), waypoint_id],
        )?;
        Ok(n)
    }

    /// Drain every still-queued injection targeting one cell: marks each
    /// `delivered` and returns them, oldest first. Exactly-once -- a row
    /// already `delivered` or `cancelled` is never returned again, so
    /// draining the same cell twice in a row returns an empty `Vec` the
    /// second time.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn drain_injections(
        &self,
        target_squad: &str,
        target_task: i64,
        target_idx: i64,
    ) -> StoreResult<Vec<PendingInjectionView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, target_squad, target_task, target_idx, payload, status, batch_id, created_at_ms, updated_at_ms, waypoint_id
             FROM pending_injections
             WHERE target_squad=? AND target_task=? AND target_idx=? AND status='queued'
             ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(
                params![target_squad, target_task, target_idx],
                Self::map_injection_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let now = now_ms();
        for row in &rows {
            self.conn.execute(
                "UPDATE pending_injections SET status='delivered', updated_at_ms=? WHERE id=? AND status='queued'",
                params![now, row.id],
            )?;
        }
        Ok(rows
            .into_iter()
            .map(|mut r| {
                r.status = "delivered".to_string();
                r
            })
            .collect())
    }

    /// A concise, plain-text description of one survey candidate's actual
    /// work, for [`survey_candidate`]'s classifier message.
    ///
    /// The classifier is otherwise handed only the candidate's opaque id
    /// (`squad-000000000005`), which carries no signal about what the work
    /// touches -- so a verdict can only be a guess. This renders the same
    /// facts a human would read off the board: the squad's label, and per
    /// task its name/project plus each cell's name, `cwd` and
    /// `prompt`/`command`.
    ///
    /// Cell text is truncated per field via [`truncate_for_survey`] so one
    /// long agent prompt can't crowd the waypoint's own guidance out of a
    /// small local model's context window.
    ///
    /// # Errors
    /// Propagates any SQLite failure; [`StoreError::NotFound`] if no such
    /// squad exists.
    pub fn describe_squad_for_survey(&self, squad_id: &str) -> StoreResult<String> {
        use std::fmt::Write as _;
        let squad = self.get_squad(squad_id)?;
        let mut out = format!("squad {squad_id}");
        if let Some(label) = squad.label.as_deref().filter(|l| !l.trim().is_empty()) {
            let _ = write!(out, " (label: {label})");
        }
        out.push('\n');
        for task in &squad.tasks {
            let _ = writeln!(out, "- task \"{}\" (project: {})", task.name, task.project);
            for cell in &task.cells {
                let what = cell
                    .prompt
                    .as_deref()
                    .map(|p| format!("prompt: {}", truncate_for_survey(p)))
                    .or_else(|| {
                        cell.command
                            .as_deref()
                            .map(|c| format!("command: {}", truncate_for_survey(c)))
                    })
                    .unwrap_or_else(|| "no prompt or command".to_string());
                let name = cell.name.as_deref().unwrap_or(&cell.id);
                let _ = writeln!(out, "  - cell \"{name}\" {what}");
                if let Some(cwd) = cell.cwd.as_deref().filter(|c| !c.trim().is_empty()) {
                    let _ = writeln!(out, "    cwd: {cwd}");
                }
            }
        }
        Ok(out)
    }

    /// [`Self::describe_squad_for_survey`]'s review counterpart: the review's
    /// name, base branch and project, the branches it stacks, and its
    /// originating squad's own description, so the classifier sees the work
    /// behind the review rather than only branch names.
    ///
    /// # Errors
    /// Propagates any SQLite failure; [`StoreError::NotFound`] if no such
    /// review exists.
    pub fn describe_review_for_survey(&self, guardian_id: &str) -> StoreResult<String> {
        use std::fmt::Write as _;
        let guardian = self.get_guardian(guardian_id)?;
        let mut out = format!("review {guardian_id} (name: {})", guardian.name);
        if let Some(project) = guardian.project.as_deref() {
            let _ = write!(out, " (project: {project})");
        }
        let _ = writeln!(out, "\nbase branch: {}", guardian.base_branch);
        for branch in &guardian.branches {
            let _ = writeln!(out, "- branch {}", branch.branch);
        }
        if let Some(squad_id) = guardian.squad_id.as_deref() {
            if let Ok(desc) = self.describe_squad_for_survey(squad_id) {
                out.push_str("originating work:\n");
                for line in desc.lines() {
                    let _ = writeln!(out, "  {line}");
                }
            }
        }
        Ok(out)
    }

    /// Dispatch [`Self::describe_squad_for_survey`]/
    /// [`Self::describe_review_for_survey`] on a candidate's kind. Degrades
    /// to the bare `kind id` line when the lookup fails, so a candidate whose
    /// rows were deleted mid-sweep still classifies (fail-closed, per this
    /// module's survey contract) instead of aborting the sweep.
    #[must_use]
    pub fn describe_candidate_for_survey(&self, kind: WaypointEntryKind, entry_id: &str) -> String {
        let described = match kind {
            WaypointEntryKind::Squad => self.describe_squad_for_survey(entry_id),
            WaypointEntryKind::Review => self.describe_review_for_survey(entry_id),
        };
        described.unwrap_or_else(|_| format!("{} {entry_id}", kind.as_str()))
    }

    fn map_injection_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingInjectionView> {
        Ok(PendingInjectionView {
            id: r.get(0)?,
            target_squad: r.get(1)?,
            target_task: r.get(2)?,
            target_idx: r.get(3)?,
            payload: r.get(4)?,
            status: r.get(5)?,
            batch_id: r.get(6)?,
            created_at_ms: r.get(7)?,
            updated_at_ms: r.get(8)?,
            waypoint_id: r.get(9)?,
        })
    }
}

/// Halt a squad's in-flight cell after a affected entry was *explicitly*
/// declared or escalated to `mode=block` on an open waypoint.
///
/// [`survey_candidate`] already does this for candidates it discovers, but an
/// explicit affected entry never goes through the survey at all -- it is
/// excluded from [`Store::waypoint_survey_candidates`] by design (a human or
/// agent named it directly, so the declaration is never second-guessed). That
/// left the registry unsignalled on every explicit path, so a squad whose
/// cell was already running kept running to completion despite being
/// hard-blocked, and only a *later* cell of that squad would ever see the
/// gate. This closes that gap so both paths halt in-flight work identically.
///
/// A no-op for `Review`-kind entries (a review has no cell of its own to
/// halt), for `advisory` mode (advisory deliberately lets work keep running),
/// and for a closed waypoint (it gates nothing, so halting would strand the
/// cell until the resume sweep un-parked it). `cancel` is itself a no-op when
/// nothing is registered under the id, so no squad/cell state check is needed
/// before calling it.
/// Re-apply this waypoint's holds to work that is already in flight.
///
/// A waypoint's gate is not a property of its affected entries alone -- it
/// also depends on whether its own affected has landed. Adding a goal to an
/// open waypoint therefore turns an open gate into a closed one for every
/// block-mode squad it affects, and those squads may be running right now.
/// Without this they run on, unaware, until they finish: the gate is only
/// consulted when a squad is *claimed*, so nothing re-reads it for work
/// already past that point.
///
/// Safe to call when nothing changed: `cancel` is a no-op for a squad with
/// no registered token, and the per-squad gate is re-checked here so a
/// waypoint whose affected is still complete halts nothing.
pub fn resignal_waypoint_holds(
    store: &Store,
    waypoint_halts: &crate::cancel::WaypointHalts,
    waypoint_id: &str,
) {
    if !store.waypoint_is_open(waypoint_id).unwrap_or(false) {
        return;
    }
    let Ok(entries) = store.list_affected_entries(waypoint_id) else {
        return;
    };
    for entry in entries {
        if entry.kind != WaypointEntryKind::Squad || entry.mode != AffectedMode::Block {
            continue;
        }
        if matches!(
            store.squad_block_gating_waypoint(&entry.entry_id),
            Ok(Some(_))
        ) {
            waypoint_halts.cancel(&entry.entry_id);
        }
    }
}

pub fn signal_explicit_block_halt(
    store: &Store,
    waypoint_halts: &crate::cancel::WaypointHalts,
    waypoint_id: &str,
    kind: WaypointEntryKind,
    entry_id: &str,
    mode: AffectedMode,
) {
    if kind != WaypointEntryKind::Squad || mode != AffectedMode::Block {
        return;
    }
    if !store.waypoint_is_open(waypoint_id).unwrap_or(false) {
        return;
    }
    waypoint_halts.cancel(entry_id);
}

/// Per-field cap on candidate-description text handed to the survey
/// classifier, in characters. Sized so a squad with several agent cells still
/// leaves a small local model (the default classifier is `ollama`/`qwen3:8b`)
/// room for the waypoint's own guidance and the reply-format spec, which the
/// verdict depends on far more than any single cell's full prompt text.
const SURVEY_DESCRIPTION_FIELD_CHARS: usize = 400;

/// Collapse one description field to a single line and cap it at
/// [`SURVEY_DESCRIPTION_FIELD_CHARS`], appending an ellipsis when truncated.
/// Newlines become spaces so a multi-line agent prompt can't forge extra
/// lines in the rendered description (the classifier reads it line-by-line),
/// and truncation respects char boundaries rather than slicing bytes.
fn truncate_for_survey(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= SURVEY_DESCRIPTION_FIELD_CHARS {
        return one_line;
    }
    let kept: String = one_line
        .chars()
        .take(SURVEY_DESCRIPTION_FIELD_CHARS)
        .collect();
    format!("{kept}...")
}

/// Build the survey's system prompt from the waypoint's own guidance prompt.
/// Mirrors `arbiter::subproject_inference_system_prompt`'s phrasing style:
/// a short role framing, the guidance verbatim, then a strict reply-format
/// spec so [`parse_survey_reply`] has a stable shape to key off. The
/// `allow_advisory` line is only present when the waypoint allows
/// de-escalation, per RAL-400 Phase 2 ("if yes and `allow_advisory` is set,
/// a second question deciding advisory de-escalation vs. keep blocking").
fn survey_system_prompt(waypoint_prompt: &str, allow_advisory: bool) -> String {
    let mode_line = if allow_advisory {
        "MODE: block or advisory -- advisory means this work only needs to be \
         informed of the guidance, not gated on it; block means it should wait \
         for/act on the guidance before proceeding. Only meaningful when \
         IMPACTED is yes; reply NONE when IMPACTED is no.\n"
    } else {
        ""
    };
    format!(
        "You are surveying one unit of work (a code review or an agent squad) \
         against the following cross-squad coordination guidance, to decide \
         whether that unit of work is actually impacted by it:\n\n\
         {waypoint_prompt}\n\n\
         The user message gives the unit of work's id followed by a \
         description of what it actually does -- its task names, and each \
         cell's prompt or command and working directory. Base your decision \
         only on that description. Judge it impacted when the work it \
         describes reads, writes, or depends on something the guidance \
         changes; judge it not impacted when the described work is in a \
         different area and the guidance would not change it. Do not infer \
         impact from the id itself, and do not assume the work touches \
         something the description does not mention.\n\n\
         Reply with exactly these lines, in this order, and nothing else -- no \
         extra commentary, no surrounding quotes:\n\
         IMPACTED: yes or no\n\
         {mode_line}\
         RATIONALE: one short sentence explaining the decision, citing what in \
         the description drove it"
    )
}

/// Parse one survey reply into a [`SurveyVerdict`], at the same robustness
/// bar as `arbiter::parse_classification_reply`: tolerant of extra
/// whitespace/blank lines, case-insensitive keywords and values, and
/// surrounding quote/period punctuation on values; a line that isn't a
/// recognized `KEY: value` pair is ignored rather than rejecting the whole
/// reply. Returns `None` when no recognizable `IMPACTED:` line is present at
/// all -- callers treat that identically to a call failure (fail closed).
fn parse_survey_reply(reply: &str, allow_advisory: bool) -> Option<SurveyVerdict> {
    let mut impacted: Option<bool> = None;
    let mut advisory = false;
    let mut rationale = String::new();
    for line in reply.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'' || c == '.');
        match key.trim().to_ascii_uppercase().as_str() {
            "IMPACTED" => {
                if val.eq_ignore_ascii_case("yes") {
                    impacted = Some(true);
                } else if val.eq_ignore_ascii_case("no") {
                    impacted = Some(false);
                }
            }
            "MODE" if allow_advisory => advisory = val.eq_ignore_ascii_case("advisory"),
            "RATIONALE" => rationale = val.to_string(),
            _ => {}
        }
    }
    let impacted = impacted?;
    let mode = if impacted && allow_advisory && advisory {
        AffectedMode::Advisory
    } else {
        AffectedMode::Block
    };
    if rationale.is_empty() {
        rationale = "model gave no rationale".to_string();
    }
    Some(SurveyVerdict {
        impacted,
        mode,
        rationale,
    })
}

/// Turn a completed survey call's outcome into a [`SurveyVerdict`], applying
/// the fail-closed rule uniformly whether the call itself failed or it
/// succeeded but returned an unparseable reply. Split out from
/// [`survey_candidate`] so the fail-closed and advisory-de-escalation
/// decision logic is unit-testable without an actual `chat_client` call.
fn resolve_survey_verdict(
    call_result: Result<String, String>,
    allow_advisory: bool,
) -> SurveyVerdict {
    match call_result {
        Ok(reply) => parse_survey_reply(&reply, allow_advisory).unwrap_or_else(|| SurveyVerdict {
            impacted: true,
            mode: AffectedMode::Block,
            rationale: format!("unparseable survey reply, failing closed: {reply:?}"),
        }),
        Err(e) => SurveyVerdict {
            impacted: true,
            mode: AffectedMode::Block,
            rationale: format!("survey call failed, failing closed: {e}"),
        },
    }
}

/// Survey one candidate against a waypoint (RAL-400 Phase 2): resolve the
/// survey agent/model, invoke the LLM once, and durably record the outcome
/// -- via [`Store::add_affected_entry`] + [`Store::set_affected_survey_result`]
/// and a Cartographer row -- on every path, including failure. The affected
/// entry is written regardless of the `impacted` verdict (not only when
/// `true`): the row is the single place both the positive and negative
/// outcome are recorded, it stops a later scheduler tick from re-surveying
/// the same still-non-terminal candidate every interval, and it is what a
/// later phase's delivery/gating logic must consult (`survey_verdict`) to
/// know whether this affected entry actually blocks/advises.
///
/// One LLM call per candidate, not batched across a waypoint's whole
/// candidate set: a batched reply covering N candidates at once would be
/// cheaper, but it couples every candidate's outcome to one reply -- a
/// single malformed/truncated batch reply (more likely as candidate count
/// grows) would force *all* of them to fail closed together, and a
/// slow/erroring call would stall every candidate in the batch rather than
/// just the one it concerns. Per-candidate calls trade some cost for that
/// failure isolation and for a Cartographer row that is attributable to
/// exactly the call that produced it; RAL-400 Phase 2 leaves this tradeoff
/// to this module and defaults to per-candidate absent a concrete cost
/// problem.
///
/// Fail-closed: any agent-resolution/call/parse failure resolves to
/// `impacted = true`, `mode = Block`, with the failure reason as the
/// rationale -- never silently dropped.
///
/// # Errors
/// Propagates a SQLite failure from reading the waypoint or persisting the
/// outcome. A survey-call failure itself is not a [`StoreError`] -- it
/// resolves to the fail-closed verdict described above instead.
pub fn survey_candidate(
    store: &crate::store_lock::StoreHandle,
    waypoint_id: &str,
    candidate: &SurveyCandidate,
    waypoint_halts: &crate::cancel::WaypointHalts,
    runner: &Arc<dyn Runner>,
) -> StoreResult<SurveyVerdict> {
    let guard = store.lock();
    let waypoint = guard.get_waypoint(waypoint_id)?;
    // `enroll_affected_entry`, not `add_affected_entry`: a discovered candidate is
    // daemon-enrolled, so it stays surveyable if this survey call fails and a
    // later sweep has to retry it.
    guard.enroll_affected_entry(
        waypoint_id,
        candidate.kind,
        &candidate.entry_id,
        AffectedMode::Block,
    )?;
    drop(guard);
    // RAL-400 Phase 3: the affected entry above just went from "not affecteded"
    // (unblocked) to `mode=block`. A squad-kind candidate may have a cell
    // actively running right now -- `cancel` is a no-op when nothing is
    // registered under this squad id, so this is safe to call unconditionally
    // rather than first checking squad/cell state. Review-kind candidates
    // never have a runner-registered token (a review has no cell of its own
    // to halt), so this is scoped to `Squad` only.
    if candidate.kind == WaypointEntryKind::Squad {
        waypoint_halts.cancel(&candidate.entry_id);
    }

    let fallback = crate::config::global_review_config();
    let agent = waypoint
        .agent
        .clone()
        .unwrap_or_else(|| fallback.default_resolver_agent().to_string());
    let model = waypoint
        .model
        .clone()
        .or_else(|| fallback.default_resolver_model().map(str::to_string));

    let system = survey_system_prompt(&waypoint.prompt, waypoint.allow_advisory);
    // The classifier cannot judge relevance from an opaque id alone, so send
    // the candidate's actual work (task names, cell prompts/commands, cwds)
    // alongside it -- see `Store::describe_candidate_for_survey`.
    let description = {
        let guard = store.lock();
        guard.describe_candidate_for_survey(candidate.kind, &candidate.entry_id)
    };
    let user = format!(
        "Unit of work under survey: {} {}\n\n{description}",
        candidate.kind.as_str(),
        candidate.entry_id
    );

    let call_result = call_survey_agent(
        store,
        runner,
        waypoint_id,
        candidate,
        &SurveyClassifier { agent, model },
        &system,
        &user,
    );
    let verdict = resolve_survey_verdict(call_result, waypoint.allow_advisory);

    let guard = store.lock();
    guard.set_affected_survey_result(waypoint_id, candidate.kind, &candidate.entry_id, &verdict)?;
    // Tell the entity's watchers what the verdict means for them. Every branch
    // notifies, including the release: someone told their work was held needs
    // to hear when it isn't any more, or the first message reads as a dead end.
    match (verdict.impacted, verdict.mode) {
        (true, AffectedMode::Block) => notify_entry_blocked(
            &guard,
            waypoint_id,
            Some(&waypoint),
            candidate.kind,
            &candidate.entry_id,
            &format!("the survey judged it impacted ({})", verdict.rationale),
        ),
        (true, AffectedMode::Advisory) => notify_entry_advised(
            &guard,
            waypoint_id,
            Some(&waypoint),
            candidate.kind,
            &candidate.entry_id,
            &verdict.rationale,
        ),
        (false, _) => {
            notify_entry_released(
                &guard,
                waypoint_id,
                candidate.kind,
                &candidate.entry_id,
                "the survey found its work is not impacted",
            );
        }
    }
    let note = crate::cartographer::Note::new("waypoints").scope("waypoint");
    let note = match candidate.kind {
        WaypointEntryKind::Squad => note.squad(&candidate.entry_id),
        WaypointEntryKind::Review => note.guardian(&candidate.entry_id),
    };
    note.emit(
        &guard,
        format!(
            "waypoint {waypoint_id} survey of {} {}: impacted={} mode={}",
            candidate.kind.as_str(),
            candidate.entry_id,
            verdict.impacted,
            verdict.mode.as_str()
        ),
        serde_json::json!({
            "waypoint_id": waypoint_id,
            "candidate_kind": candidate.kind.as_str(),
            "candidate_id": candidate.entry_id,
            "impacted": verdict.impacted,
            "mode": verdict.mode.as_str(),
            "rationale": verdict.rationale,
        }),
    );
    Ok(verdict)
}

/// Scheduler-owned periodic sweep (RAL-400 Phase 2): survey every waiting
/// candidate on every open waypoint. Must only ever be invoked from
/// `scheduler::run_loop`'s periodic tick -- never synchronously inside the
/// waypoint-submit HTTP handler, since a single survey call can take seconds
/// and a waypoint may have many candidates. The candidate lookups are cheap
/// synchronous store reads done on the calling (scheduler) thread, but each
/// actual survey call is dispatched onto its own spawned thread -- mirroring
/// `crate::pr::poll_forge_reorders` -- so the scheduler loop is never
/// blocked for the cumulative duration of every open waypoint's every
/// candidate's LLM call.
pub fn run_pending_surveys(
    store: &crate::store_lock::StoreHandle,
    waypoint_halts: &crate::cancel::WaypointHalts,
    runner: &Arc<dyn Runner>,
) {
    let waypoint_ids = {
        let guard = store.lock();
        log_swallowed(
            &guard,
            "list open waypoints",
            guard.list_open_waypoint_ids(),
        )
        .unwrap_or_default()
    };
    let mut budget = SURVEY_MAX_PER_SWEEP;
    for waypoint_id in waypoint_ids {
        let candidates = {
            let guard = store.lock();
            log_swallowed(
                &guard,
                &format!("waypoint {waypoint_id} list survey candidates"),
                guard.waypoint_survey_candidates(&waypoint_id),
            )
            .unwrap_or_default()
        };
        let total = candidates.len();
        let taken = total.min(budget);
        if taken < total {
            // Never let a cap look like completed coverage: say what was
            // deferred and why, since the deferred entries stay blocked (NULL
            // verdict) in the meantime and someone will want to know why.
            let guard = store.lock();
            crate::cartographer::Note::new("waypoints")
                .level(crate::logging::LogLevel::WARNING)
                .scope("waypoint")
                .emit(
                    &guard,
                    format!(
                        "waypoint {waypoint_id} surveyed {taken} of {total} candidates this \
                         sweep; {} deferred to the next one",
                        total - taken
                    ),
                    serde_json::json!({
                        "waypoint_id": waypoint_id,
                        "surveyed": taken,
                        "candidates": total,
                        "per_sweep_cap": SURVEY_MAX_PER_SWEEP,
                    }),
                );
        }
        for candidate in candidates.into_iter().take(taken) {
            let store = std::sync::Arc::clone(store);
            let waypoint_id = waypoint_id.clone();
            let waypoint_halts = waypoint_halts.clone();
            let runner = Arc::clone(runner);
            std::thread::spawn(move || {
                let result =
                    survey_candidate(&store, &waypoint_id, &candidate, &waypoint_halts, &runner);
                if result.is_ok() {
                    return;
                }
                log_swallowed(
                    &store.lock(),
                    &format!(
                        "waypoint {waypoint_id} survey of {} {}",
                        candidate.kind.as_str(),
                        candidate.entry_id
                    ),
                    result,
                );
            });
        }
        budget -= taken;
        if budget == 0 {
            break;
        }
    }
}

/// Scheduler-owned periodic sweep (RAL-400 Phase 3): the other half of a
/// waypoint halt. `run_cell_worker`'s `is_waypoint_halted()` branch stops a
/// cell the moment its squad-kind affected entry becomes `mode=block`, but
/// nothing else in that codepath ever hands the cell back -- a waypoint can
/// close, de-escalate to advisory, or lose its last blocking affected entry at
/// any later time, with no single call site to hook a "resume now" trigger
/// onto (unlike the halt itself, which is driven directly by
/// [`survey_candidate`] flipping a affected entry to `block`). So this sweep
/// re-checks every currently-halted cell on the same cadence as
/// [`run_pending_surveys`], purely synchronous store reads/writes (no LLM
/// call, no thread-spawn needed).
///
/// A cell only resumes once [`Store::squad_block_gating_waypoint`] no longer
/// names a blocking waypoint for its squad, and only if the DB still shows it
/// `Running` -- guarding against a cell that moved on through some other path
/// (a manual restart, say) while still carrying a stale
/// `waypoint_halted_at_ms`. Resuming sets the cell back to `pending`; a live
/// squad worker notices via the same Detached-revival reconciliation
/// `resume_detached_cell` relies on (`scheduler::execute_squad_inner`'s
/// `reclaimed_detached` check), so only a squad whose worker has already
/// exited (`!cancellations.is_active`) needs its own row nudged back to
/// `Pending` to be picked up by a fresh `scheduler::tick`.
pub fn run_pending_waypoint_resumes(
    store: &crate::store_lock::StoreHandle,
    cancellations: &crate::cancel::Cancellations,
) {
    let halted = {
        let guard = store.lock();
        log_swallowed(
            &guard,
            "list waypoint-halted cells",
            guard.waypoint_halted_cells(),
        )
        .unwrap_or_default()
    };
    let mut resumed_squads: BTreeSet<String> = BTreeSet::new();
    for (squad_id, task_idx, idx) in halted {
        let guard = store.lock();
        if log_swallowed(
            &guard,
            &format!("squad {squad_id} blocking-waypoint check before resume"),
            guard.squad_block_gating_waypoint(&squad_id),
        )
        .flatten()
        .is_some()
        {
            continue;
        }
        if !matches!(
            guard.cell_state(&squad_id, task_idx, idx),
            Ok(Some(crate::store::NodeState::Running))
        ) {
            continue;
        }
        // Mirror `server::resume_automation`'s pairing: a session id is only
        // worth forcing a resume onto if one was actually captured live
        // before the halt (a cell halted before the agent ever reported a
        // session id has nothing of its own to continue) -- `run_cell_worker`
        // falls back to ordinary fresh-dispatch/cross-cell-sharing logic
        // otherwise, same as any other cell with no session to resume.
        if matches!(
            guard.get_cell_agent_resume(&squad_id, task_idx, idx),
            Ok((_, _, Some(_)))
        ) {
            log_swallowed(
                &guard,
                &format!("squad {squad_id} cell {task_idx}/{idx} force-resume own session"),
                guard.set_force_resume_own_session(&squad_id, task_idx, idx),
            );
        }
        if log_swallowed(
            &guard,
            &format!("squad {squad_id} cell {task_idx}/{idx} resume from waypoint halt"),
            guard.resume_waypoint_halted_cell(&squad_id, task_idx, idx),
        )
        .is_some()
        {
            crate::cartographer::Note::new("waypoints")
                .scope("waypoint")
                .squad(&squad_id)
                .emit(
                    &guard,
                    format!(
                        "squad {squad_id} cell {task_idx}/{idx} resumed: no waypoint blocks it any more"
                    ),
                    serde_json::json!({
                        "squad_id": squad_id,
                        "task_idx": task_idx,
                        "cell_idx": idx,
                    }),
                );
            resumed_squads.insert(squad_id);
        }
    }
    for squad_id in resumed_squads {
        if !cancellations.is_active(&squad_id) {
            let guard = store.lock();
            log_swallowed(
                &guard,
                &format!("squad {squad_id} re-queue after waypoint resume"),
                guard.set_squad_state(&squad_id, crate::store::SquadState::Pending),
            );
        }
    }
}

/// Author attributed to a waypoint's delivered feedback messages, mirroring
/// `ci_watch.rs`'s `AUTO_FIX_AUTHOR`/`MANUAL_PR_FIX_AUTHOR` naming precedent
/// for automation-originated guardian messages.
pub const WAYPOINT_FEEDBACK_AUTHOR: &str = "Waypoint";

/// The topmost (highest-position) enabled branch with a worktree already
/// built, if any. This mirrors the readiness bar `guardian_merge::start_feedback`
/// itself enforces (its synchronous 409 "run the review merge before giving
/// feedback" path) -- checked here first so a not-yet-built review is simply
/// skipped for this sweep (left `Undelivered` for a later one to retry)
/// rather than surfaced as a `Failed` affected entry.
fn topmost_ready_branch(
    guardian: &crate::guardian::GuardianView,
) -> Option<&crate::guardian::BranchView> {
    guardian
        .branches
        .iter()
        .filter(|b| b.enabled && b.worktree.is_some())
        .max_by_key(|b| b.position)
}

/// Render a waypoint's guidance as one feedback message body.
fn waypoint_feedback_text(waypoint: &WaypointView) -> String {
    match &waypoint.label {
        Some(label) => format!("Waypoint \"{label}\": {}", waypoint.prompt),
        None => waypoint.prompt.clone(),
    }
}

/// RAL-400 Phase 5: render a waypoint's current bearing list into a compact,
/// clearly-delimited block for the ghost-fold delivery path (see this
/// module's doc comment). Returns `None` for an empty list rather than an
/// empty/near-empty block, so a halted cell with no bearings yet doesn't get
/// a note that just says nothing.
///
/// Preserves the distinction between a completed change (`commit_id`/
/// `commit_summary` populated -- the bearing is tied to an actual commit)
/// and a requested/proposed one (those fields `None` -- the bearing is pure
/// guidance text) by only appending the `(completed: ...)` marker when the
/// commit fields are present; the raw `summary` text -- wherever "required"
/// vs. "proposed" wording lives -- is always rendered verbatim, never
/// stripped or normalized away. Commit summaries and entity links are
/// included as investigation leads, matching `WAYPOINT_SYSTEM_PROMPT`'s
/// instruction that they are leads, not a substitute for inspecting the
/// cell's own working state.
#[must_use]
pub fn render_bearing_block(waypoint_id: &str, bearings: &[BearingView]) -> Option<String> {
    if bearings.is_empty() {
        return None;
    }
    let mut out = format!("--- Waypoint `{waypoint_id}` bearings ---\n");
    for bearing in bearings {
        out.push_str(&format!(
            "- [{} {}] {}",
            bearing.producer_kind.as_str(),
            bearing.producer_id,
            bearing.summary
        ));
        if let (Some(commit_id), Some(commit_summary)) = (
            bearing.commit_id.as_deref(),
            bearing.commit_summary.as_deref(),
        ) {
            out.push_str(&format!(
                " (completed: commit {commit_id} -- {commit_summary})"
            ));
        }
        if let Some(entity_uri) = bearing.entity_uri.as_deref() {
            out.push_str(&format!(" [{entity_uri}]"));
        }
        out.push('\n');
    }
    out.push_str("--- End waypoint bearings ---\n");
    Some(out)
}

/// Scheduler-owned periodic sweep (RAL-400 Phase 4): deliver every open
/// waypoint's guidance to every affected entry judged impacted. A `NULL` or
/// `"impacted"` `survey_verdict` both count as impacted here (mirroring
/// [`Store::squad_block_gating_waypoint`]'s own fail-closed reading of that
/// column); only an explicit `"not_impacted"` skips delivery.
///
/// - `Review`-kind entries are delivered through the *existing* review
///   feedback path -- `guardian_merge::start_feedback`, the same function
///   `POST /api/guardians/{id}/branches/{branch_id}/feedback` calls -- rather
///   than a new delivery mechanism. That function already does its own
///   `feedback:<branch_id>` worktree-lease queueing behind an in-flight
///   rebase; this sweep only decides *when* to call it, never reimplements
///   that serialization.
/// - `Squad`-kind entries never get a direct delivery call: a squad has no
///   review worktree to write feedback into. If such a squad has already
///   gone terminal (done/failed/cancelled) -- the "done-but-unreviewed" case,
///   where the squad finished before a review ever formed for it and before
///   this ticket's gating could apply -- its affected entry is marked
///   `via-restack` so the UI can say honestly that its guidance will only
///   reach it later, folded into the restack/rebase that runs once its
///   eventual review is built (at which point
///   `Store::transition_squad_affected_entries_to_review` converts the entry
///   to `Review`-kind and this sweep starts delivering to it directly). A
///   still-running squad's entry is left untouched -- there is nothing to do
///   for it yet.
///
/// Mirrors [`run_pending_surveys`]'s shape: cheap synchronous store reads on
/// the calling (scheduler) thread, gating what work happens; the potentially
/// slow part (`start_feedback`'s spawned background thread) is not owned by
/// this function's call stack at all, so no thread-spawn is needed here.
pub fn run_pending_deliveries(
    store: &crate::store_lock::StoreHandle,
    runner: &Arc<dyn Runner>,
    cancellations: &crate::cancel::Cancellations,
) {
    let waypoint_ids = {
        let guard = store.lock();
        log_swallowed(
            &guard,
            "list open waypoints",
            guard.list_open_waypoint_ids(),
        )
        .unwrap_or_default()
    };
    for waypoint_id in waypoint_ids {
        let (waypoint, entries) = {
            let guard = store.lock();
            let Ok(waypoint) = guard.get_waypoint(&waypoint_id) else {
                continue;
            };
            let entries = log_swallowed(
                &guard,
                &format!("waypoint {waypoint_id} list affected entries"),
                guard.list_affected_entries(&waypoint_id),
            )
            .unwrap_or_default();
            (waypoint, entries)
        };
        for entry in entries {
            if entry.delivery_status != DeliveryStatus::Undelivered {
                continue;
            }
            if entry.survey_verdict.as_deref() == Some("not_impacted") {
                continue;
            }
            match entry.kind {
                WaypointEntryKind::Review => {
                    deliver_to_review(
                        store,
                        runner,
                        cancellations,
                        &waypoint_id,
                        &waypoint,
                        &entry.entry_id,
                    );
                }
                WaypointEntryKind::Squad => {
                    mark_done_but_unreviewed_squad(store, &waypoint_id, &entry.entry_id);
                }
            }
        }
    }
}

/// Deliver one waypoint's guidance to one review's topmost ready branch via
/// the existing feedback path, recording the outcome on the affected entry.
/// Leaves the entry `Undelivered` (for a later sweep to retry) if the
/// guardian has no ready branch yet, or if `start_feedback` itself reports
/// `404` (stale affected entry, guardian/branch since gone) or `409` (branch
/// has no worktree yet -- an ordinary not-built-yet race, not a failure); any
/// other reply status is recorded as `Failed`.
fn deliver_to_review(
    store: &crate::store_lock::StoreHandle,
    runner: &Arc<dyn Runner>,
    cancellations: &crate::cancel::Cancellations,
    waypoint_id: &str,
    waypoint: &WaypointView,
    guardian_id: &str,
) {
    let branch_id = {
        let guard = store.lock();
        let Ok(guardian) = guard.get_guardian(guardian_id) else {
            return;
        };
        match topmost_ready_branch(&guardian) {
            Some(branch) => branch.id.clone(),
            None => return,
        }
    };
    let reply = crate::guardian_merge::start_feedback(
        Arc::clone(store),
        Arc::clone(runner),
        cancellations.clone(),
        guardian_id,
        &branch_id,
        waypoint_feedback_text(waypoint),
        Some(WAYPOINT_FEEDBACK_AUTHOR.to_string()),
        None,
    );
    let status = match reply.status {
        202 => DeliveryStatus::Delivered,
        404 | 409 => return,
        _ => DeliveryStatus::Failed,
    };
    let guard = store.lock();
    log_swallowed(
        &guard,
        &format!("waypoint {waypoint_id} record delivery status for review {guardian_id}"),
        guard.set_affected_delivery_status(
            waypoint_id,
            WaypointEntryKind::Review,
            guardian_id,
            status,
        ),
    );
    let note = crate::cartographer::Note::new("waypoints")
        .scope("waypoint")
        .guardian(guardian_id);
    note.emit(
        &guard,
        format!(
            "waypoint {waypoint_id} delivered feedback to review {guardian_id}: status={}",
            status.as_str()
        ),
        serde_json::json!({
            "waypoint_id": waypoint_id,
            "guardian_id": guardian_id,
            "branch_id": branch_id,
            "delivery_status": status.as_str(),
        }),
    );
}

/// Mark a squad-kind affected entry `via-restack` once its squad has gone
/// terminal without a review ever having formed for it (the "done-but-
/// unreviewed" case). A no-op if the squad is not yet terminal, or is gone
/// entirely (treated the same as "not yet terminal" -- nothing to mark).
fn mark_done_but_unreviewed_squad(
    store: &crate::store_lock::StoreHandle,
    waypoint_id: &str,
    squad_id: &str,
) {
    let guard = store.lock();
    let terminal = matches!(guard.squad_state(squad_id), Ok(state) if state.is_terminal());
    if !terminal {
        return;
    }
    log_swallowed(
        &guard,
        &format!("waypoint {waypoint_id} mark squad {squad_id} via-restack"),
        guard.set_affected_delivery_status(
            waypoint_id,
            WaypointEntryKind::Squad,
            squad_id,
            DeliveryStatus::ViaRestack,
        ),
    );
    let note = crate::cartographer::Note::new("waypoints")
        .scope("waypoint")
        .squad(squad_id);
    note.emit(
        &guard,
        format!(
            "waypoint {waypoint_id} squad {squad_id} finished before its review formed -- via-restack"
        ),
        serde_json::json!({
            "waypoint_id": waypoint_id,
            "squad_id": squad_id,
            "delivery_status": DeliveryStatus::ViaRestack.as_str(),
        }),
    );
}

/// Record how one unit of work answered every open waypoint affecting it.
///
/// Shared by the squad and review paths so an answer means the same thing
/// whichever produced it: the decision lands on the affected entry, the
/// message is appended as a bearing (so it shows up in the waypoint's own
/// feed and in the bearing block later work reads), and each waypoint is
/// re-checked for closure, since an answer can be the last thing it was
/// waiting on.
///
/// Returns the waypoints that accepted the answer.
pub fn record_waypoint_answer(
    store: &crate::store_lock::StoreHandle,
    kind: WaypointEntryKind,
    entry_id: &str,
    decision: BearingDecision,
    message: &str,
) -> Vec<String> {
    let guard = store.lock();
    let waypoints = log_swallowed(
        &guard,
        &format!("list open waypoints affecting {} {entry_id}", kind.as_str()),
        match kind {
            WaypointEntryKind::Squad => guard.open_waypoints_affecting_squad(entry_id),
            WaypointEntryKind::Review => guard.open_waypoints_affecting_review(entry_id),
        },
    )
    .unwrap_or_default();
    let mut answered = Vec::new();
    for waypoint_id in &waypoints {
        if log_swallowed(
            &guard,
            &format!(
                "waypoint {waypoint_id} record answer from {} {entry_id}",
                kind.as_str()
            ),
            guard.set_affected_bearing_decision(waypoint_id, kind, entry_id, decision),
        )
        .is_none()
        {
            continue;
        }
        log_swallowed(
            &guard,
            &format!(
                "waypoint {waypoint_id} append answer bearing from {} {entry_id}",
                kind.as_str()
            ),
            guard.append_waypoint_bearing(waypoint_id, kind, entry_id, message, None, None, None),
        );
        let note = crate::cartographer::Note::new("waypoints").scope("waypoint");
        let note = match kind {
            WaypointEntryKind::Squad => note.squad(entry_id),
            WaypointEntryKind::Review => note.guardian(entry_id),
        };
        note.emit(
            &guard,
            format!(
                "waypoint {waypoint_id} answered by {} {entry_id}: {}",
                kind.as_str(),
                decision.as_str()
            ),
            serde_json::json!({
                "waypoint_id": waypoint_id,
                "kind": kind.as_str(),
                "entry_id": entry_id,
                "decision": decision.as_str(),
                "message": message,
            }),
        );
        answered.push(waypoint_id.clone());
    }
    drop(guard);
    // An answer can be the last thing a waypoint was waiting for.
    for waypoint_id in &answered {
        let guard = store.lock();
        log_swallowed(
            &guard,
            &format!("waypoint {waypoint_id} auto-close check after answer"),
            guard.maybe_auto_close_waypoint(waypoint_id),
        );
    }
    answered
}

/// The guidance block a cell carries back into its own prompt when it resumes
/// from a waypoint hold.
///
/// A halted cell is released once the waypoint's affected has landed, and its
/// worktree is rebased onto that change on the way back in. Both of those are
/// invisible to the agent: it just gets re-invoked. Without this it resumes
/// with no idea a waypoint ever held it, let alone what the waypoint wanted.
///
/// `rebased` says whether the release actually moved the worktree. When it
/// did, the change is already present locally and the likely correct action
/// is none -- saying so is cheaper than letting the agent rediscover it by
/// diffing, and stops it re-implementing work its own tree already contains.
pub fn render_resume_guidance(waypoint: &WaypointView, rebased: bool) -> String {
    let which = match &waypoint.label {
        Some(label) => format!("waypoint \"{label}\" ({})", waypoint.id),
        None => format!("waypoint {}", waypoint.id),
    };
    let mut out = format!(
        "--- Waypoint guidance ({which}) ---\nYour work was held while this waypoint's own \
         change was still being made. That change has now landed, and you are being resumed.\n\n\
         {}\n\n",
        waypoint.prompt.trim()
    );
    if rebased {
        out.push_str(
            "Your worktree was rebased onto the branch carrying that change before you were \
             resumed, so you most likely already have it. Check whether it is present before \
             doing anything: if it is, there is nothing here for you to implement, and saying \
             so is the whole of what is wanted. Act only on whatever part of this your own work \
             still does not reflect.\n\n",
        );
    } else {
        out.push_str(
            "Your worktree was not rebased, so inspect the current state of the code yourself \
             rather than assuming the described change is present locally.\n\n",
        );
    }
    out.push_str(
        "Before you finish, answer this waypoint with a line of the form \
         `RALPHUS_BEARING: <accepted|rejected|deferred>: <one line>`. Declining is a legitimate \
         answer; staying silent is not, and is what keeps your work held.\n\
         --- End waypoint guidance ---\n",
    );
    out
}

/// Render a closed waypoint's stand-down notice text (RAL-400 Phase 6),
/// shared by both the squad (`notify_watchers`) and review (`start_feedback`)
/// delivery paths.
fn stand_down_text(waypoint: &WaypointView) -> String {
    match &waypoint.label {
        Some(label) => {
            format!(
                "Waypoint \"{label}\" has closed -- no further action is needed for its guidance."
            )
        }
        None => {
            "This waypoint has closed -- no further action is needed for its guidance.".to_string()
        }
    }
}

/// Scheduler-owned periodic sweep (RAL-400 Phase 6): send each closed
/// waypoint's *advisory*-mode affected entries a one-time stand-down notice
/// once their waypoint has closed (auto- or manually). `Block`-mode entries
/// never get one -- once their waypoint closes, gating simply lifts (see
/// `Store::squad_block_gating_waypoint`'s live `state='open'` filter), which
/// is itself the signal; a separate notice would be redundant.
///
/// Idempotent per entry via `stand_down_at_ms`
/// ([`Store::mark_affected_entry_stood_down`]) -- reopening a waypoint does not
/// reset it (see [`Store::reopen_waypoint`]'s doc comment), so a later
/// re-close never re-sends a notice that already went out.
///
/// - `Review`-kind entries reuse the same feedback path as ordinary guidance
///   delivery (`guardian_merge::start_feedback` via [`stand_down_review`]),
///   left pending for a later sweep exactly like [`deliver_to_review`] when
///   there is no ready branch yet or `start_feedback` reports a non-`202`
///   status.
/// - `Squad`-kind entries go out via [`Store::notify_watchers`]
///   ([`stand_down_squad`]) -- a squad has no review worktree to write
///   feedback into, and unlike delivery there is no live cell to resume, so
///   a plain informational mailbox message at `squad:{id}` is enough (v1
///   scope).
///
/// Mirrors [`run_pending_deliveries`]'s shape: cheap synchronous store reads
/// on the calling (scheduler) thread gate what work happens; the
/// potentially slow part (`start_feedback`'s spawned background thread) is
/// not owned by this function's call stack at all.
pub fn run_pending_stand_down_notices(store: &crate::store_lock::StoreHandle) {
    let waypoint_ids = {
        let guard = store.lock();
        log_swallowed(
            &guard,
            "list closed waypoints",
            guard.list_closed_waypoint_ids(),
        )
        .unwrap_or_default()
    };
    for waypoint_id in waypoint_ids {
        let (waypoint, entries) = {
            let guard = store.lock();
            let Ok(waypoint) = guard.get_waypoint(&waypoint_id) else {
                continue;
            };
            let entries = log_swallowed(
                &guard,
                &format!("waypoint {waypoint_id} list affected entries"),
                guard.list_affected_entries(&waypoint_id),
            )
            .unwrap_or_default();
            (waypoint, entries)
        };
        for entry in entries {
            if entry.mode != AffectedMode::Advisory || entry.stand_down_at_ms.is_some() {
                continue;
            }
            match entry.kind {
                WaypointEntryKind::Review => {
                    stand_down_review(store, &waypoint_id, &waypoint, &entry.entry_id);
                }
                WaypointEntryKind::Squad => {
                    stand_down_squad(store, &waypoint_id, &waypoint, &entry.entry_id);
                }
            }
        }
    }
}

/// Send one review affected entry's stand-down notice to the review's watchers.
///
/// Deliberately a plain mailbox notification, not the feedback path. The
/// feedback path (`guardian_merge::start_feedback`) is how *guidance* reaches
/// a review, and it works by dispatching the resolver agent -- it moves the
/// review through `merging`/`actioning` and runs a real, billed agent turn.
/// Spending that to deliver "no further action is needed" inverts the cost of
/// the message against its content, and churns a settled review's status to
/// say nothing. A stand-down is for the humans watching the review, so it goes
/// where they are looking.
///
/// Dropping the feedback call also removes everything that existed only to
/// serve it: the ready-branch precondition, and the `404`/`409` retry
/// tolerance for a guardian that was not ready yet. A mailbox row has no such
/// races, so the notice is sent once and marked, unconditionally.
fn stand_down_review(
    store: &crate::store_lock::StoreHandle,
    waypoint_id: &str,
    waypoint: &WaypointView,
    guardian_id: &str,
) {
    let guard = store.lock();
    log_swallowed(
        &guard,
        &format!("waypoint {waypoint_id} stand-down notice to review {guardian_id}"),
        guard.notify_watchers_with_context(
            crate::monitor::NotifiableEventKind::WaypointAdvised,
            &format!("guardian:{guardian_id}"),
            crate::mailbox::MailboxPriority::Normal,
            &stand_down_text(waypoint),
            None,
            None,
            None,
        ),
    );
    log_swallowed(
        &guard,
        &format!("waypoint {waypoint_id} mark review {guardian_id} stood down"),
        guard.mark_affected_entry_stood_down(waypoint_id, WaypointEntryKind::Review, guardian_id),
    );
    crate::cartographer::Note::new("waypoints")
        .scope("waypoint")
        .guardian(guardian_id)
        .emit(
            &guard,
            format!("waypoint {waypoint_id} sent stand-down notice to review {guardian_id}"),
            serde_json::json!({
                "waypoint_id": waypoint_id,
                "guardian_id": guardian_id,
            }),
        );
}

/// Send one squad affected entry's stand-down notice, addressed to the cells
/// this waypoint actually advised so their own watchers receive it.
///
/// A watch matches when the *watched* entity covers the message's entity
/// ([`crate::entity_uri::EntityUri::covers`], parent to child only). A single
/// row at `squad:{id}` therefore reaches squad-level watchers but never
/// someone watching one specific cell -- so the people closest to the advised
/// work were the ones who never heard the guidance had lapsed. Emitting per
/// advised cell inverts that: cell watchers match exactly, and squad- and
/// task-level watchers still match by covering those cells.
///
/// The audience is bounded to cells that actually had guidance queued for
/// them ([`Store::waypoint_advised_cells`]), not every cell in the squad -- a
/// cell that was never told anything does not need telling that it no longer
/// applies. A squad the waypoint advised but never reached a cell of falls
/// back to one squad-addressed row.
///
/// Proof steps are not addressed separately: a waypoint advises work, not the
/// verification of it, and a proof-scope URI does not cover its cell, so a
/// per-proof row would multiply the mail without reaching an audience the
/// cell rows do not already serve.
fn stand_down_squad(
    store: &crate::store_lock::StoreHandle,
    waypoint_id: &str,
    waypoint: &WaypointView,
    squad_id: &str,
) {
    let guard = store.lock();
    let advised = log_swallowed(
        &guard,
        &format!("waypoint {waypoint_id} list advised cells of squad {squad_id}"),
        guard.waypoint_advised_cells(waypoint_id, squad_id),
    )
    .unwrap_or_default();
    let text = stand_down_text(waypoint);
    if advised.is_empty() {
        log_swallowed(
            &guard,
            &format!("waypoint {waypoint_id} stand-down notice to squad {squad_id}"),
            guard.notify_watchers(
                crate::monitor::NotifiableEventKind::WaypointAdvised,
                &format!("squad:{squad_id}"),
                crate::mailbox::MailboxPriority::Normal,
                &text,
                Some(squad_id),
            ),
        );
    } else {
        for (task_idx, idx) in &advised {
            log_swallowed(
                &guard,
                &format!(
                    "waypoint {waypoint_id} stand-down notice to cell {squad_id}:{task_idx}:{idx}"
                ),
                guard.notify_watchers(
                    crate::monitor::NotifiableEventKind::WaypointAdvised,
                    &format!("cell:{squad_id}:{task_idx}:{idx}"),
                    crate::mailbox::MailboxPriority::Normal,
                    &text,
                    Some(squad_id),
                ),
            );
        }
    }
    log_swallowed(
        &guard,
        &format!("waypoint {waypoint_id} mark squad {squad_id} stood down"),
        guard.mark_affected_entry_stood_down(waypoint_id, WaypointEntryKind::Squad, squad_id),
    );
    crate::cartographer::Note::new("waypoints")
        .scope("waypoint")
        .squad(squad_id)
        .emit(
            &guard,
            format!("waypoint {waypoint_id} sent stand-down notice to squad {squad_id}"),
            serde_json::json!({
                "waypoint_id": waypoint_id,
                "squad_id": squad_id,
                "advised_cells": advised.len(),
            }),
        );
}

/// Most survey classifications dispatched in a single sweep, across every
/// open waypoint.
///
/// The sweep spawns a thread per candidate, and a candidate's scope can be
/// `RepoWide` -- so in a single-project repo one waypoint's candidate set is
/// "every non-terminal squad and open review in the project." With the
/// direct-chat transport that was merely a burst of HTTP calls; now that a
/// terminal agent is a supported classifier, each one can be a real
/// `claude-code`/`codex` process with a real bill, so an unbounded fan-out is
/// no longer acceptable.
///
/// Deferring rather than dropping is safe: the sweep is idempotent and
/// re-runs on `scheduler::WAYPOINT_SURVEY_INTERVAL`, and an unsurveyed
/// candidate keeps its NULL verdict, which the gate already treats as
/// blocking. So the only cost of the cap is latency, never a missed gate.
///
/// This cap, rather than batching candidates into one call, is the deliberate
/// answer to survey cost -- batching would make a single failure fail *every*
/// candidate in the batch closed, widening the blast radius of the mechanism
/// the ticket most depends on being right. See the "Batch-vs-per-candidate"
/// section of `.agent/waypoints-phase0-decisions.md`.
const SURVEY_MAX_PER_SWEEP: usize = 8;

/// The `EntityUri` string for one affected entry, as the mailbox and watch
/// machinery address it.
#[must_use]
pub fn affected_entry_uri(kind: WaypointEntryKind, entry_id: &str) -> String {
    match kind {
        WaypointEntryKind::Squad => format!("squad:{entry_id}"),
        WaypointEntryKind::Review => format!("guardian:{entry_id}"),
    }
}

/// Notify a affected entry's watchers that an open waypoint is now **holding**
/// it, so work that is blocked says so instead of sitting silently.
///
/// Carries remediation per RAL-502 (this is a blocked state): both ways out --
/// de-escalate the entry, or close the waypoint -- are named, since neither is
/// discoverable from the entity's own page.
///
/// `detail` distinguishes *why* it is held (gated at submit pending survey, a
/// confirmed `block` verdict, a held approval), because the same entity can be
/// notified more than once as that progresses and an undifferentiated repeat
/// reads as spam rather than news.
pub fn notify_entry_blocked(
    store: &Store,
    waypoint_id: &str,
    waypoint: Option<&WaypointView>,
    kind: WaypointEntryKind,
    entry_id: &str,
    detail: &str,
) {
    let which = waypoint.and_then(|w| w.label.clone()).map_or_else(
        || format!("waypoint {waypoint_id}"),
        |l| format!("waypoint \"{l}\""),
    );
    let message = format!(
        "This {} ({entry_id}) is held by open {which}: {detail}. It stays held until the waypoint \
         closes or this affected entry is set to advisory.",
        kind.as_str()
    );
    let remediation = crate::mailbox::Remediation::SuggestedCommand {
        command: format!("ralphus waypoint affected mode {waypoint_id} {entry_id} advisory"),
        purpose: "release this entry without closing the waypoint, if it only needs to be aware \
                  of the guidance rather than wait for it"
            .to_string(),
    };
    let squad_scope = match kind {
        WaypointEntryKind::Squad => Some(entry_id),
        WaypointEntryKind::Review => None,
    };
    log_swallowed(
        store,
        &format!(
            "waypoint {waypoint_id} blocked notice to {} {entry_id}",
            kind.as_str()
        ),
        store.notify_watchers_with_remediation(
            crate::monitor::NotifiableEventKind::WaypointBlocked,
            &affected_entry_uri(kind, entry_id),
            crate::mailbox::MailboxPriority::Normal,
            &message,
            &remediation,
            squad_scope,
            None,
            None,
        ),
    );
}

/// Notify a affected entry's watchers that a waypoint's guidance applies to it
/// in `advisory` mode -- not held, but expected to account for the guidance.
///
/// Informational, so it goes through `notify_watchers_with_context` rather
/// than the remediation-carrying variant: nothing is stuck and there is no
/// corrective command to run.
pub fn notify_entry_advised(
    store: &Store,
    waypoint_id: &str,
    waypoint: Option<&WaypointView>,
    kind: WaypointEntryKind,
    entry_id: &str,
    detail: &str,
) {
    let which = waypoint.and_then(|w| w.label.clone()).map_or_else(
        || format!("waypoint {waypoint_id}"),
        |l| format!("waypoint \"{l}\""),
    );
    let message = format!(
        "Open {which} advises this {} ({entry_id}): {detail}. It is not held -- inspect the \
         current state of the code rather than assuming the described change is already present, \
         and respond as applicable.",
        kind.as_str()
    );
    let squad_scope = match kind {
        WaypointEntryKind::Squad => Some(entry_id),
        WaypointEntryKind::Review => None,
    };
    log_swallowed(
        store,
        &format!(
            "waypoint {waypoint_id} advisory notice to {} {entry_id}",
            kind.as_str()
        ),
        store.notify_watchers_with_context(
            crate::monitor::NotifiableEventKind::WaypointAdvised,
            &affected_entry_uri(kind, entry_id),
            crate::mailbox::MailboxPriority::Normal,
            &message,
            squad_scope,
            None,
            None,
        ),
    );
}

/// Notify that a waypoint has stopped holding a affected entry because the
/// survey found no impact. Uses the entity's own ordinary status-change kind,
/// not a waypoint-specific one: nothing is blocked and nothing is advised, the
/// entity simply became runnable again, and someone who was told it was held
/// needs to hear that it isn't.
pub fn notify_entry_released(
    store: &Store,
    waypoint_id: &str,
    kind: WaypointEntryKind,
    entry_id: &str,
    reason: &str,
) {
    let (event, squad_scope) = match kind {
        WaypointEntryKind::Squad => (
            crate::monitor::NotifiableEventKind::SquadAttributesChanged,
            Some(entry_id),
        ),
        WaypointEntryKind::Review => (
            crate::monitor::NotifiableEventKind::ReviewStatusChanged,
            None,
        ),
    };
    log_swallowed(
        store,
        &format!(
            "waypoint {waypoint_id} release notice to {} {entry_id}",
            kind.as_str()
        ),
        store.notify_watchers_with_context(
            event,
            &affected_entry_uri(kind, entry_id),
            crate::mailbox::MailboxPriority::Normal,
            &format!(
                "Waypoint {waypoint_id} no longer holds this {} ({entry_id}): {reason}.",
                kind.as_str()
            ),
            squad_scope,
            None,
            None,
        ),
    );
    // The mailbox only reaches watchers, and an entry often has none. A hold
    // lifting is a change to the waypoint's own state, so it belongs in the
    // waypoint's feed either way -- otherwise its timeline shows work being
    // held and never shows it let go.
    let note = crate::cartographer::Note::new("waypoints").scope("waypoint");
    let note = match kind {
        WaypointEntryKind::Squad => note.squad(entry_id),
        WaypointEntryKind::Review => note.guardian(entry_id),
    };
    note.emit(
        store,
        format!(
            "waypoint {waypoint_id} no longer holds {} {entry_id}: {reason}",
            kind.as_str()
        ),
        serde_json::json!({
            "waypoint_id": waypoint_id,
            "kind": kind.as_str(),
            "entry_id": entry_id,
            "reason": reason,
        }),
    );
}

/// Wall-clock cap on one survey classification run through the subprocess
/// runner. A terminal agent has no built-in bound, and a survey is a small
/// read-only question -- so a run that outlives this is stuck, not thorough.
/// The direct-chat transport needs no equivalent: `ureq` carries its own
/// timeouts.
const SURVEY_RUNNER_TIMEOUT_SECS: u64 = 300;

/// The agent/model pair a waypoint's survey classification runs as, after the
/// waypoint's own `agent`/`model` have been resolved against the project's
/// resolver defaults.
struct SurveyClassifier {
    agent: String,
    model: Option<String>,
}

/// Whether `backend` is one the direct chat-API transport can call itself
/// ([`chat_client::call_direct`]). Everything else -- `claude-code`, `codex`,
/// `pi`, and any custom `[agent.profiles.*]` -- is a terminal executable and
/// has to go through the subprocess runner instead.
///
/// Kept as one predicate rather than inlined at the branch so the two
/// transports can never disagree about which backend they own.
fn direct_chat_handles(backend: &str) -> bool {
    matches!(
        backend.to_lowercase().as_str(),
        "claude" | "anthropic" | "ollama"
    )
}

/// The working directory a survey classification should run in: the
/// candidate's own, so `.ralphus.toml` agent profiles and project config
/// resolve the same way they would for that work's real cells.
///
/// A squad uses its first cell's `cwd`; a review uses its guardian's
/// `git_root`. `None` when neither is recorded, which makes the caller fall
/// back to the direct-chat transport (a terminal agent cannot be spawned
/// without somewhere to spawn it).
fn survey_candidate_cwd(store: &Store, kind: WaypointEntryKind, entry_id: &str) -> Option<String> {
    match kind {
        WaypointEntryKind::Squad => store.get_squad(entry_id).ok().and_then(|squad| {
            squad.tasks.iter().find_map(|task| {
                task.cells
                    .iter()
                    .find_map(|cell| cell.cwd.clone().filter(|c| !c.trim().is_empty()))
            })
        }),
        WaypointEntryKind::Review => store
            .get_guardian(entry_id)
            .ok()
            .map(|g| g.git_root)
            .filter(|r| !r.trim().is_empty()),
    }
}

/// Run one survey classification through whichever transport its configured
/// agent needs, and return the agent's reply text.
///
/// `claude`/`anthropic`/`ollama` go through the direct chat API as before.
/// A terminal-executable agent (`claude-code`, `codex`, `pi`, a custom
/// profile) goes through [`crate::runner::Runner`] -- the same path a
/// `prompt`-kind proof step already uses to ask an agent a question and read
/// its answer back, via [`RunnerSpec::for_proof`] and `RunnerResult::summary`.
///
/// Without this, naming a terminal agent produced a hard error on every
/// single call. Because the survey is fail-closed, that error resolved to
/// impacted + block, so such a waypoint silently blocked every piece of work
/// it covered for as long as it stayed open -- and `claude-code` is the name
/// a user reaches for first, since it is what cells use everywhere else.
///
/// The system prompt goes in `system_prompt` and the candidate description in
/// the prompt body, matching how a prompt-kind proof splits its own
/// instructions from its question.
fn call_survey_agent(
    store: &crate::store_lock::StoreHandle,
    runner: &Arc<dyn Runner>,
    waypoint_id: &str,
    candidate: &SurveyCandidate,
    classifier: &SurveyClassifier,
    system: &str,
    user: &str,
) -> Result<String, String> {
    let SurveyClassifier { agent, model } = classifier;
    let (agent, model) = (agent.as_str(), model.as_deref());
    let cwd = {
        let guard = store.lock();
        survey_candidate_cwd(&guard, candidate.kind, &candidate.entry_id)
    };
    // No cwd means no profile/config context to resolve against and nowhere to
    // spawn a process, so the only transport left is the direct API.
    let Some(cwd) = cwd else {
        return chat_client::call_direct(
            agent,
            model,
            system,
            &[ChatMessage {
                role: "user",
                content: user.to_string(),
                image: None,
            }],
        );
    };
    let selection = crate::scheduler::resolve_agent_selection(store, agent, &cwd)?;
    if direct_chat_handles(&selection.backend) {
        return chat_client::call_direct(
            &selection.backend,
            model.or(selection.model.as_deref()),
            system,
            &[ChatMessage {
                role: "user",
                content: user.to_string(),
                image: None,
            }],
        );
    }

    runner.preflight_agent(&selection.backend, selection.executable.as_deref(), None)?;
    let mut spec = crate::runner::RunnerSpec::for_proof(
        &candidate.entry_id,
        "waypoint-survey",
        &format!("survey-{waypoint_id}"),
        &cwd,
        user,
        &selection.backend,
        model.or(selection.model.as_deref()),
        Some(SURVEY_RUNNER_TIMEOUT_SECS),
        None,
        None,
    );
    spec.system_prompt = Some(system.to_string());
    spec.executable = selection.executable.clone();
    spec.env_overrides = selection.env.clone().into_iter().collect();
    let result = runner.run(&spec);
    if let Some(error) = result.error.as_deref().filter(|e| !e.trim().is_empty()) {
        return Err(error.to_string());
    }
    if result.summary.trim().is_empty() {
        return Err(format!(
            "agent {:?} returned no reply to classify (status {:?})",
            selection.backend, result.status
        ));
    }
    Ok(result.summary)
}

/// Render a cell's prior [`crate::prophecy::ProphecyView`]s into a block for
/// the redo ghost-fold, or `None` when the cell recorded none.
///
/// Prophecies are ordinarily a *human*-facing channel: the agent-facing
/// contract in `runner.rs` says a prophecy is "for the human in the eventual
/// pull request -- not by the next agent", and the general dispatch path
/// deliberately never injects them. A redo is the one place that framing does
/// not fit: the operator is explicitly asking for this work to be regenerated
/// *because* something it depended on changed, and the whole point of redoing
/// rather than resubmitting is to keep what the last run learned. Discarding
/// the previous agent's own recorded discoveries, decisions, hazards and
/// deferrals there would throw away the most valuable thing the run produced.
///
/// So this is scoped to redo only -- it does not change what any ordinary
/// dispatch injects, and prophecies remain human/PR-facing everywhere else.
#[must_use]
pub fn render_prophecy_block(prophecies: &[crate::prophecy::ProphecyView]) -> Option<String> {
    if prophecies.is_empty() {
        return None;
    }
    let mut out = String::from("--- Findings from the previous run of this cell ---\n");
    out.push_str(
        "You have run this work before. These are the insights that run recorded. Treat them as \
         leads, not as established fact about the current tree -- the code may have moved since, \
         and this redo exists because something it depended on changed.\n",
    );
    for prophecy in prophecies {
        out.push_str(&format!("- [{}] {}\n", prophecy.kind, prophecy.body.trim()));
    }
    out.push_str("--- End findings from the previous run ---\n");
    Some(out)
}

/// Render drained [`PendingInjectionView`] payloads into one prompt-prefix
/// block. Each payload is already a rendered bearing block (see
/// [`Store::queue_advisory_bearing_injections`]), so this only frames and
/// concatenates them in arrival order.
///
/// Mirrors [`render_bearing_block`]'s framing so an agent sees the same shape
/// whether guidance arrived via a blocking halt's ghost-fold or an advisory
/// injection, and matches `WAYPOINT_SYSTEM_PROMPT`'s contract that injected
/// waypoint information is expected rather than anomalous.
#[must_use]
pub fn render_injection_block(injections: &[PendingInjectionView], rebased: bool) -> String {
    let mut out = String::from("--- Advisory waypoint guidance ---\n");
    out.push_str(
        "The following guidance was published while this work was in flight. It is advisory: \
         it does not block this cell. Inspect the current state of the code rather than \
         assuming the described changes are already present locally, and respond as \
         applicable to your own task.\n\n",
    );
    if rebased {
        // Without this, an agent reads guidance describing a change and sets
        // out to make it -- having just been rebased onto a branch where it
        // is already made. The likely correct action here is none at all, and
        // saying so is cheaper than letting it rediscover that by diffing.
        out.push_str(
            "You have most likely received this change already: your work was rebased onto the \
             branch carrying it before this ran. So treat this as notice that something moved \
             underneath you rather than as a task -- check whether the change is already \
             present, and if it is, there is nothing here for you to do beyond saying so. \
             Act only on whatever part of the guidance your own work still does not \
             reflect.\n\n",
        );
    }
    for injection in injections {
        out.push_str(injection.payload.trim());
        out.push('\n');
    }
    out.push_str("--- End advisory waypoint guidance ---\n\n");
    out
}

/// The message body for a stale-work notice. Names the waypoint whose closure
/// triggered it, so a reader can tell *which* coordination point their
/// finished work predates.
fn stale_notice_text(waypoint: &WaypointView, kind: WaypointEntryKind, entry_id: &str) -> String {
    let which = match &waypoint.label {
        Some(label) => format!("waypoint \"{label}\""),
        None => format!("waypoint {}", waypoint.id),
    };
    format!(
        "This {} ({entry_id}) finished while {} was still open and had judged it impacted -- so \
         the work landed without that waypoint's own changes and may now be stale. Nothing has \
         been re-run.",
        kind.as_str(),
        which
    )
}

/// Scheduler-owned periodic sweep: flag any affected entry whose work finished
/// **while its waypoint was still open** and whose survey had judged it
/// `impacted`, so a human can decide whether to redo it.
///
/// This is the "work was already done when the waypoint's own changes landed"
/// case. Keying on the waypoint still being *open* is what makes the rule
/// correct: work that finished before the waypoint closed cannot have
/// incorporated its guidance, whereas work that only became claimable *after*
/// the close already ran against the landed changes and is not stale. Sweeping
/// closed waypoints instead would flag exactly that second, healthy group --
/// every squad the gate correctly held until the waypoint closed.
///
/// In practice the entries this catches are the ones that were allowed to keep
/// running: `advisory`-mode work (never halted, by definition), and work
/// de-escalated from `block` to `advisory` mid-flight. `block`-mode work
/// cannot finish while its waypoint is open, so it is never flagged -- which
/// is the correct outcome, not a gap.
///
/// Deliberately advisory: it never re-runs anything on its own. A waypoint in
/// a single-project repo has `Scope::RepoWide`, so it can legitimately cover
/// every squad in the project -- auto-redoing on that basis would re-run an
/// unbounded amount of finished work and spend real money with no one asking.
/// Acting on a flag is [`redo_affected_entry`], via `ralphus waypoint redo`.
///
/// Costs no LLM calls: it only reads verdicts the survey already recorded.
/// That is also its one limitation -- work that was *already terminal before
/// the waypoint existed* was never a survey candidate (terminal work is
/// excluded from [`Store::waypoint_survey_candidates`] by construction), so it
/// has no verdict to key off and is not flagged. Catching that would mean
/// surveying the full history of terminal squads against every open waypoint,
/// which is unbounded; it is left out on purpose.
///
/// Idempotent per entry via `stale_at_ms`
/// ([`Store::mark_affected_entry_stale`]), which a redo clears, so an entry
/// already dealt with is never re-flagged.
pub fn run_pending_stale_notices(store: &crate::store_lock::StoreHandle) {
    let waypoint_ids = {
        let guard = store.lock();
        log_swallowed(
            &guard,
            "list open waypoints",
            guard.list_open_waypoint_ids(),
        )
        .unwrap_or_default()
    };
    for waypoint_id in waypoint_ids {
        let (waypoint, entries) = {
            let guard = store.lock();
            let Ok(waypoint) = guard.get_waypoint(&waypoint_id) else {
                continue;
            };
            let entries = log_swallowed(
                &guard,
                &format!("waypoint {waypoint_id} list affected entries"),
                guard.list_affected_entries(&waypoint_id),
            )
            .unwrap_or_default();
            (waypoint, entries)
        };
        for entry in entries {
            if entry.survey_verdict.as_deref() != Some("impacted") || entry.stale_at_ms.is_some() {
                continue;
            }
            let guard = store.lock();
            if !log_swallowed(
                &guard,
                &format!(
                    "waypoint {waypoint_id} terminal check for {} {}",
                    entry.kind.as_str(),
                    entry.entry_id
                ),
                guard.affected_entry_work_is_terminal(entry.kind, &entry.entry_id),
            )
            .unwrap_or(false)
            {
                continue;
            }
            if !log_swallowed(
                &guard,
                &format!(
                    "waypoint {waypoint_id} mark {} {} stale",
                    entry.kind.as_str(),
                    entry.entry_id
                ),
                guard.mark_affected_entry_stale(&waypoint_id, entry.kind, &entry.entry_id),
            )
            .unwrap_or(false)
            {
                continue;
            }
            // A squad can be redone directly; a review cannot (it has no
            // cells of its own), so point that case at the squad that
            // produced it.
            //
            // This goes out via `notify_watchers_with_context`, not
            // `notify_watchers_with_remediation`: a stale flag is an ordinary
            // status change, not a failure or blocked state (nothing is stuck
            // and nothing has failed), and the remediation-carrying variant
            // asserts it is only used for the failure/blocked event kinds. The
            // next-step command is therefore part of the message body, the
            // same way the new-waypoint affected notice states its own.
            let next_step = match entry.kind {
                WaypointEntryKind::Squad => format!(
                    " To re-run it with its prior findings and this waypoint's bearings \
                     carried into the new run: `ralphus waypoint redo {waypoint_id} {}`.",
                    entry.entry_id
                ),
                WaypointEntryKind::Review => format!(
                    " A review has no cells of its own to re-run -- inspect this waypoint's \
                     bearings with `ralphus waypoint get {waypoint_id}`, then redo the squad \
                     behind this review if its work needs regenerating."
                ),
            };
            let entity_uri = match entry.kind {
                WaypointEntryKind::Squad => format!("squad:{}", entry.entry_id),
                WaypointEntryKind::Review => format!("guardian:{}", entry.entry_id),
            };
            let squad_scope = match entry.kind {
                WaypointEntryKind::Squad => Some(entry.entry_id.as_str()),
                WaypointEntryKind::Review => None,
            };
            let body = format!(
                "{}{next_step}",
                stale_notice_text(&waypoint, entry.kind, &entry.entry_id)
            );
            log_swallowed(
                &guard,
                &format!("waypoint {waypoint_id} stale notice to {entity_uri}"),
                guard.notify_watchers_with_context(
                    crate::monitor::NotifiableEventKind::SquadAttributesChanged,
                    &entity_uri,
                    crate::mailbox::MailboxPriority::Normal,
                    &body,
                    squad_scope,
                    None,
                    None,
                ),
            );
            let note = crate::cartographer::Note::new("waypoints").scope("waypoint");
            let note = match entry.kind {
                WaypointEntryKind::Squad => note.squad(&entry.entry_id),
                WaypointEntryKind::Review => note.guardian(&entry.entry_id),
            };
            note.emit(
                &guard,
                format!(
                    "waypoint {waypoint_id} flagged finished {} {} as possibly stale",
                    entry.kind.as_str(),
                    entry.entry_id
                ),
                serde_json::json!({
                    "waypoint_id": waypoint_id,
                    "entry_kind": entry.kind.as_str(),
                    "entry_id": entry.entry_id,
                }),
            );
        }
    }
}

/// Redo one stale-flagged squad affected entry: fold the waypoint's bearings
/// into every cell's ghost note, reset the squad to `pending`, and clear the
/// flag. Returns the squads this dirtied downstream (same as
/// [`Store::restart_squad`]).
///
/// This is the "keep the insights, forward them to a new generation" path.
/// Two existing mechanisms do the actual carrying, so nothing new is invented
/// here:
///
/// - [`Store::restart_squad`] resets cells to `pending` without touching the
///   `ghosts` table, and a ghost is keyed by `(squad, task, cell)` -- so the
///   previous run's own self-summarized findings survive the reset and the
///   scheduler's existing ghost-context prepend feeds them to the new run.
/// - [`render_bearing_block`] + [`Store::upsert_ghost`] add the waypoint's
///   bearings on top, exactly as the Phase 5 halt path already does, and
///   `upsert_ghost` *merges* rather than overwrites, so the bearings augment
///   the prior findings instead of clobbering them.
///
/// Rejects a `Review`-kind entry: a review has no cells of its own to re-run,
/// and redoing the squad behind it is the meaningful action.
///
/// # Errors
/// [`StoreError::NotFound`] if the waypoint or affected entry doesn't exist;
/// [`StoreError::InvalidTransition`] for a review-kind entry; otherwise
/// propagates any SQLite failure.
pub fn redo_affected_entry(
    store: &Store,
    waypoint_id: &str,
    entry_id: &str,
) -> StoreResult<Vec<String>> {
    let _ = store.get_waypoint(waypoint_id)?;
    let entry = store
        .list_affected_entries(waypoint_id)?
        .into_iter()
        .find(|e| e.entry_id == entry_id)
        .ok_or(StoreError::NotFound)?;
    if entry.kind != WaypointEntryKind::Squad {
        return Err(StoreError::InvalidTransition(format!(
            "affected entry {entry_id} on waypoint {waypoint_id} is a review, which has no cells \
             of its own to re-run -- redo the squad that produced it instead"
        )));
    }

    // Fold the bearings and the previous run's own recorded findings in before
    // the reset, so both are in place by the time the scheduler can claim the
    // squad again. Per cell, because prophecies are recorded per cell.
    let bearings = store.list_waypoint_bearings(waypoint_id)?;
    let bearing_text = render_bearing_block(waypoint_id, &bearings);
    let squad = store.get_squad(entry_id)?;
    let mut carried_prophecies = 0usize;
    for (task_idx, task) in squad.tasks.iter().enumerate() {
        for (idx, cell) in task.cells.iter().enumerate() {
            let uri = crate::ghost::cell_uri(entry_id, task_idx as i64, idx as i64);
            let prophecies = store.list_prophecies_for_entity(&uri)?;
            carried_prophecies += prophecies.len();
            let mut fold = String::new();
            if let Some(block) = render_prophecy_block(&prophecies) {
                fold.push_str(&block);
            }
            if let Some(block) = bearing_text.as_deref() {
                if !fold.is_empty() {
                    fold.push('\n');
                }
                fold.push_str(block);
            }
            if fold.is_empty() {
                continue;
            }
            let revision = crate::ghost::current_revision(cell.cwd.as_deref().unwrap_or_default());
            store.upsert_ghost(
                &uri,
                crate::ghost::KIND_CELL,
                Some(entry_id),
                None,
                &fold,
                revision.as_deref(),
            )?;
        }
    }

    let dirtied = store.restart_squad(entry_id)?;
    log_swallowed(
        store,
        &format!("waypoint {waypoint_id} clear stale flag on squad {entry_id}"),
        store.clear_affected_entry_stale(waypoint_id, WaypointEntryKind::Squad, entry_id),
    );
    crate::cartographer::Note::new("waypoints")
        .scope("waypoint")
        .squad(entry_id)
        .emit(
            store,
            format!("waypoint {waypoint_id} redo re-queued squad {entry_id}"),
            serde_json::json!({
                "waypoint_id": waypoint_id,
                "squad_id": entry_id,
                "bearings": bearings.len(),
                "carried_prophecies": carried_prophecies,
                "dirtied_squads": dirtied,
            }),
        );
    Ok(dirtied)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cancel::Cancellations;
    use crate::store::{NodeState, SquadState};
    use crate::store_lock::StoreMutex;

    /// Records the answer a blocking affected entry owes its waypoint, so a
    /// test about *terminality* can reach phase 2 without also restating the
    /// bearing contract each time. See `Store::affected_have_answered`.
    fn answer(store: &Store, waypoint_id: &str, kind: WaypointEntryKind, entry_id: &str) {
        store
            .set_affected_bearing_decision(waypoint_id, kind, entry_id, BearingDecision::Accepted)
            .unwrap();
    }

    /// How many mailbox messages exist, so a test can assert that an action
    /// said something without caring which watcher it reached.
    fn mailbox_len(store: &Store) -> i64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM mailbox_messages", [], |r| r.get(0))
            .unwrap_or(0)
    }

    fn open_waypoint(store: &Store, id: &str) {
        store
            .create_waypoint(id, Some("label"), "prompt text", None, None, false)
            .unwrap();
    }

    fn insert_bare_squad(store: &Store, id: &str, state: SquadState) {
        let now = now_ms();
        store
            .conn
            .execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES(?,?,?,?)",
                params![id, state.as_str(), now, now],
            )
            .unwrap();
    }

    fn insert_bare_task(store: &Store, squad_id: &str, idx: i64, project: &str) {
        store
            .conn
            .execute(
                "INSERT INTO tasks(squad_id, idx, name, project, state) VALUES(?,?,?,?,'pending')",
                params![squad_id, idx, format!("task-{idx}"), project],
            )
            .unwrap();
    }

    fn insert_bare_cell(
        store: &Store,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        review_guardian_id: Option<&str>,
        review_branch: Option<&str>,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, review_guardian_id, review_branch)
                 VALUES(?,?,?,?,'claude-code','pending',?,?)",
                params![squad_id, task_idx, idx, format!("s{task_idx}-{idx}"), review_guardian_id, review_branch],
            )
            .unwrap();
    }

    fn insert_cell_subproject(
        store: &Store,
        squad_id: &str,
        task_idx: i64,
        idx: i64,
        subproject: &str,
        inferred: bool,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO cell_subprojects(squad_id, task_idx, idx, subproject, inferred, created_at_ms) VALUES(?,?,?,?,?,?)",
                params![squad_id, task_idx, idx, subproject, inferred, now_ms()],
            )
            .unwrap();
    }

    fn insert_bare_guardian(store: &Store, id: &str) {
        let now = now_ms();
        store
            .conn
            .execute(
                "INSERT INTO guardians(id, name, base_branch, git_root, status, created_at_ms, updated_at_ms) VALUES(?,?,?,?,'collecting',?,?)",
                params![id, "review", "main", "/tmp/repo", now, now],
            )
            .unwrap();
    }

    #[test]
    fn affected_add_remove_review_and_squad_entries() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();

        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, WaypointEntryKind::Review);
        assert_eq!(entries[0].mode, AffectedMode::Block);
        assert_eq!(entries[1].kind, WaypointEntryKind::Squad);
        assert_eq!(entries[1].mode, AffectedMode::Advisory);

        assert!(
            store
                .remove_affected_entry("waypoint-1", WaypointEntryKind::Review, "guardian-1")
                .unwrap()
        );
        assert!(
            !store
                .remove_affected_entry("waypoint-1", WaypointEntryKind::Review, "guardian-1")
                .unwrap()
        );
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, WaypointEntryKind::Squad);
    }

    #[test]
    fn re_adding_a_affected_entry_updates_mode_in_place() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert_eq!(entries.len(), 1, "re-adding must not duplicate the row");
        assert_eq!(entries[0].mode, AffectedMode::Advisory);
    }

    #[test]
    fn auto_close_requires_every_affected_entry_terminal_squad_and_review_alike() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        let squad_id = "squad-1";
        insert_bare_squad(&store, squad_id, SquadState::Pending);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                squad_id,
                AffectedMode::Block,
            )
            .unwrap();
        answer(&store, "waypoint-1", WaypointEntryKind::Squad, squad_id);
        assert!(!store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert!(store.waypoint_is_open("waypoint-1").unwrap());

        // Direct SQL (not `set_squad_state`) so this exercises
        // `maybe_auto_close_waypoint` in isolation, independent of the
        // `set_squad_state` auto-close hook covered separately below.
        store
            .conn
            .execute(
                "UPDATE squads SET state='done' WHERE id=?",
                params![squad_id],
            )
            .unwrap();
        assert!(store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert!(!store.waypoint_is_open("waypoint-1").unwrap());
    }

    #[test]
    fn auto_close_stays_open_while_any_entry_is_non_terminal() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        let squad_a = "squad-a";
        let squad_b = "squad-b";
        insert_bare_squad(&store, squad_a, SquadState::Pending);
        insert_bare_squad(&store, squad_b, SquadState::Pending);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                squad_a,
                AffectedMode::Block,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                squad_b,
                AffectedMode::Block,
            )
            .unwrap();
        store.set_squad_state(squad_a, SquadState::Done).unwrap();
        store.set_squad_state(squad_b, SquadState::Running).unwrap();

        assert!(!store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert!(store.waypoint_is_open("waypoint-1").unwrap());
    }

    #[test]
    fn auto_close_never_fires_on_an_empty_affected() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        assert!(!store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert!(store.waypoint_is_open("waypoint-1").unwrap());
    }

    #[test]
    fn set_squad_state_hook_auto_closes_but_failed_does_not() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        answer(&store, "waypoint-1", WaypointEntryKind::Squad, "squad-1");

        store
            .set_squad_state("squad-1", SquadState::Failed)
            .unwrap();
        assert!(
            store.waypoint_is_open("waypoint-1").unwrap(),
            "a failed squad may still be restarted, so it must not auto-close a waypoint it gates"
        );

        store.set_squad_state("squad-1", SquadState::Done).unwrap();
        assert!(
            !store.waypoint_is_open("waypoint-1").unwrap(),
            "set_squad_state's auto-close hook must fire without an explicit maybe_auto_close_waypoint call"
        );
    }

    #[test]
    fn set_guardian_status_hook_auto_closes_but_merge_failed_does_not() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Block,
            )
            .unwrap();
        answer(
            &store,
            "waypoint-1",
            WaypointEntryKind::Review,
            "guardian-1",
        );

        store
            .set_guardian_status("guardian-1", GuardianStatus::MergeFailed, Some("conflict"))
            .unwrap();
        assert!(
            store.waypoint_is_open("waypoint-1").unwrap(),
            "a merge-failed review may still be retried, so it must not auto-close a waypoint it gates"
        );

        store
            .set_guardian_status("guardian-1", GuardianStatus::Merged, None)
            .unwrap();
        assert!(
            !store.waypoint_is_open("waypoint-1").unwrap(),
            "set_guardian_status's auto-close hook must fire without an explicit maybe_auto_close_waypoint call"
        );
    }

    #[test]
    fn cancelled_counts_as_terminal_for_both_squad_and_review_entries() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_guardian(&store, "guardian-1");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Block,
            )
            .unwrap();
        answer(&store, "waypoint-1", WaypointEntryKind::Squad, "squad-1");
        answer(
            &store,
            "waypoint-1",
            WaypointEntryKind::Review,
            "guardian-1",
        );

        store
            .set_guardian_status("guardian-1", GuardianStatus::Cancelled, None)
            .unwrap();
        assert!(store.waypoint_is_open("waypoint-1").unwrap());

        store
            .set_squad_state("squad-1", SquadState::Cancelled)
            .unwrap();
        assert!(
            !store.waypoint_is_open("waypoint-1").unwrap(),
            "a cancelled squad and a cancelled review must both count as terminal"
        );
    }

    #[test]
    fn two_waypoints_sharing_a_affected_entry_close_independently() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        open_waypoint(&store, "waypoint-2");
        insert_bare_squad(&store, "squad-shared", SquadState::Pending);
        insert_bare_squad(&store, "squad-only-2", SquadState::Pending);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-shared",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-2",
                WaypointEntryKind::Squad,
                "squad-shared",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-2",
                WaypointEntryKind::Squad,
                "squad-only-2",
                AffectedMode::Block,
            )
            .unwrap();
        answer(
            &store,
            "waypoint-1",
            WaypointEntryKind::Squad,
            "squad-shared",
        );
        answer(
            &store,
            "waypoint-2",
            WaypointEntryKind::Squad,
            "squad-shared",
        );
        answer(
            &store,
            "waypoint-2",
            WaypointEntryKind::Squad,
            "squad-only-2",
        );

        store
            .set_squad_state("squad-shared", SquadState::Done)
            .unwrap();
        assert!(
            !store.waypoint_is_open("waypoint-1").unwrap(),
            "waypoint-1's only affected entry is now terminal, so it should auto-close"
        );
        assert!(
            store.waypoint_is_open("waypoint-2").unwrap(),
            "waypoint-2 still has a non-terminal squad-only-2 entry, so it must stay open"
        );

        store
            .set_squad_state("squad-only-2", SquadState::Done)
            .unwrap();
        assert!(
            !store.waypoint_is_open("waypoint-2").unwrap(),
            "waypoint-2's last affected entry is now terminal, so it should auto-close too"
        );
    }

    #[test]
    fn manual_close_overrides_non_terminal_affected_and_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();

        assert!(store.close_waypoint_manually("waypoint-1").unwrap());
        assert!(!store.waypoint_is_open("waypoint-1").unwrap());
        assert!(
            !store.close_waypoint_manually("waypoint-1").unwrap(),
            "closing an already-closed waypoint must be a no-op"
        );
    }

    #[test]
    fn reopen_is_idempotent_and_does_not_reset_stand_down_timestamps() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        insert_bare_squad(&store, "squad-1", SquadState::Pending);

        assert!(
            !store.reopen_waypoint("waypoint-1").unwrap(),
            "reopening an already-open waypoint must be a no-op"
        );

        store
            .mark_affected_entry_stood_down("waypoint-1", WaypointEntryKind::Squad, "squad-1")
            .unwrap();
        let stood_down_at = store.list_affected_entries("waypoint-1").unwrap()[0]
            .stand_down_at_ms
            .unwrap();

        assert!(store.close_waypoint_manually("waypoint-1").unwrap());
        assert!(store.reopen_waypoint("waypoint-1").unwrap());
        assert!(store.waypoint_is_open("waypoint-1").unwrap());
        assert!(
            !store.reopen_waypoint("waypoint-1").unwrap(),
            "reopening an already-open waypoint must be a no-op"
        );

        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert_eq!(
            entries[0].stand_down_at_ms,
            Some(stood_down_at),
            "reopening must never reset a stand-down timestamp that already fired"
        );
    }

    #[test]
    fn mark_affected_entry_stood_down_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();

        store
            .mark_affected_entry_stood_down("waypoint-1", WaypointEntryKind::Squad, "squad-1")
            .unwrap();
        let first = store.list_affected_entries("waypoint-1").unwrap()[0]
            .stand_down_at_ms
            .unwrap();
        store
            .mark_affected_entry_stood_down("waypoint-1", WaypointEntryKind::Squad, "squad-1")
            .unwrap();
        let second = store.list_affected_entries("waypoint-1").unwrap()[0]
            .stand_down_at_ms
            .unwrap();
        assert_eq!(
            first, second,
            "a second call must keep the original timestamp"
        );
    }

    #[test]
    fn bearings_are_appended_in_order_and_never_mutated() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        let b1 = store
            .append_waypoint_bearing(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                "first summary",
                None,
                None,
                None,
            )
            .unwrap();
        let b2 = store
            .append_waypoint_bearing(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                "second summary",
                Some("squad:squad-1"),
                Some("deadbeef"),
                Some("fix: thing"),
            )
            .unwrap();

        assert!(b2.id > b1.id);
        let bearings = store.list_waypoint_bearings("waypoint-1").unwrap();
        assert_eq!(bearings.len(), 2);
        assert_eq!(bearings[0].id, b1.id);
        assert_eq!(bearings[0].summary, "first summary");
        assert_eq!(bearings[0].entity_uri, None);
        assert_eq!(bearings[0].commit_id, None);
        assert_eq!(bearings[1].id, b2.id);
        assert_eq!(bearings[1].summary, "second summary");
        assert_eq!(bearings[1].entity_uri.as_deref(), Some("squad:squad-1"));
        assert_eq!(bearings[1].commit_id.as_deref(), Some("deadbeef"));
        assert_eq!(bearings[1].commit_summary.as_deref(), Some("fix: thing"));
    }

    #[test]
    fn bearings_are_scoped_per_waypoint_with_no_cross_leakage() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        open_waypoint(&store, "waypoint-2");

        store
            .append_waypoint_bearing(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                "for one",
                None,
                None,
                None,
            )
            .unwrap();
        store
            .append_waypoint_bearing(
                "waypoint-2",
                WaypointEntryKind::Squad,
                "squad-2",
                "for two",
                None,
                None,
                None,
            )
            .unwrap();

        let one = store.list_waypoint_bearings("waypoint-1").unwrap();
        let two = store.list_waypoint_bearings("waypoint-2").unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].summary, "for one");
        assert_eq!(two.len(), 1);
        assert_eq!(two[0].summary, "for two");
    }

    #[test]
    fn render_bearing_block_returns_none_for_an_empty_list() {
        assert_eq!(render_bearing_block("waypoint-1", &[]), None);
    }

    #[test]
    fn render_bearing_block_preserves_completed_requested_and_proposed_distinction() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        // Completed: tied to an actual commit.
        let completed = store
            .append_waypoint_bearing(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-completed",
                "renamed the shared helper to `resolve_thing`",
                Some("squad:squad-completed"),
                Some("deadbeef"),
                Some("refactor: rename helper"),
            )
            .unwrap();
        // Requested: pure guidance text, no commit yet.
        let requested = store
            .append_waypoint_bearing(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-requested",
                "please rename your call sites to use the new helper name",
                None,
                None,
                None,
            )
            .unwrap();
        // Proposed/planned: pure guidance text, phrased as not-yet-decided.
        let proposed = store
            .append_waypoint_bearing(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-proposed",
                "considering removing the helper entirely in a future pass",
                None,
                None,
                None,
            )
            .unwrap();

        let bearings = store.list_waypoint_bearings("waypoint-1").unwrap();
        let block = render_bearing_block("waypoint-1", &bearings).expect("non-empty block");

        assert!(block.starts_with("--- Waypoint `waypoint-1` bearings ---\n"));
        assert!(block.trim_end().ends_with("--- End waypoint bearings ---"));

        // Completed bearing: commit marker present, entity link present.
        assert!(
            block.contains("renamed the shared helper to `resolve_thing`"),
            "{block}"
        );
        assert!(
            block.contains("(completed: commit deadbeef -- refactor: rename helper)"),
            "{block}"
        );
        assert!(block.contains("[squad:squad-completed]"), "{block}");

        // Requested bearing: raw summary present verbatim, no commit marker.
        assert!(
            block.contains("please rename your call sites to use the new helper name"),
            "{block}"
        );

        // Proposed bearing: raw summary present verbatim, no commit marker.
        assert!(
            block.contains("considering removing the helper entirely in a future pass"),
            "{block}"
        );

        // None of the three collapse into an identical rendering -- the
        // completed marker distinguishes bearing 1 from bearings 2 and 3,
        // and each bearing's own summary text distinguishes it from the
        // others (nothing is normalized away).
        let ids = [completed.id, requested.id, proposed.id];
        assert_eq!(ids.len(), 3, "sanity: three distinct bearings recorded");
        let completed_marker_count = block.matches("(completed:").count();
        assert_eq!(
            completed_marker_count, 1,
            "only the completed bearing should carry a commit marker: {block}"
        );
    }

    #[test]
    fn injection_drain_is_exactly_once() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue_injection("squad-1", 0, 0, "payload a", None, Some("waypoint-1"))
            .unwrap();
        store
            .enqueue_injection("squad-1", 0, 0, "payload b", None, Some("waypoint-1"))
            .unwrap();

        let drained = store.drain_injections("squad-1", 0, 0).unwrap();
        assert_eq!(drained.len(), 2);
        assert!(drained.iter().all(|r| r.status == "delivered"));

        let drained_again = store.drain_injections("squad-1", 0, 0).unwrap();
        assert!(
            drained_again.is_empty(),
            "a second drain must return nothing"
        );
    }

    #[test]
    fn injection_cancel_wins_over_a_later_drain() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue_injection(
                "squad-1",
                0,
                0,
                "payload",
                Some("batch-1"),
                Some("waypoint-1"),
            )
            .unwrap();

        let cancelled = store.cancel_injection_batch("batch-1").unwrap();
        assert_eq!(cancelled, 1);

        let drained = store.drain_injections("squad-1", 0, 0).unwrap();
        assert!(
            drained.is_empty(),
            "a cancelled injection must never be drained"
        );

        let cancelled_again = store.cancel_injection_batch("batch-1").unwrap();
        assert_eq!(
            cancelled_again, 0,
            "cancelling an already-cancelled batch is a no-op"
        );
    }

    #[test]
    fn injection_batches_only_affect_their_own_batch() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue_injection(
                "squad-1",
                0,
                0,
                "payload a",
                Some("batch-1"),
                Some("waypoint-1"),
            )
            .unwrap();
        store
            .enqueue_injection(
                "squad-1",
                0,
                0,
                "payload b",
                Some("batch-2"),
                Some("waypoint-1"),
            )
            .unwrap();

        store.cancel_injection_batch("batch-1").unwrap();
        let drained = store.drain_injections("squad-1", 0, 0).unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].payload, "payload b");
    }

    #[test]
    fn injection_drain_is_scoped_per_target_cell() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue_injection("squad-1", 0, 0, "for cell a", None, Some("waypoint-1"))
            .unwrap();
        store
            .enqueue_injection("squad-1", 0, 1, "for cell b", None, Some("waypoint-1"))
            .unwrap();

        let drained_a = store.drain_injections("squad-1", 0, 0).unwrap();
        assert_eq!(drained_a.len(), 1);
        assert_eq!(drained_a[0].payload, "for cell a");

        let drained_b = store.drain_injections("squad-1", 0, 1).unwrap();
        assert_eq!(drained_b.len(), 1);
        assert_eq!(drained_b[0].payload, "for cell b");
    }

    #[test]
    fn aggregate_scope_unions_resolved_subprojects() {
        let cells = vec![
            SubprojectResolution::Resolved {
                subprojects: vec!["core".to_string()],
                inferred: false,
            },
            SubprojectResolution::Resolved {
                subprojects: vec!["daemon".to_string(), "core".to_string()],
                inferred: true,
            },
        ];
        assert_eq!(
            aggregate_scope(&cells),
            Scope::Areas(BTreeSet::from(["core".to_string(), "daemon".to_string()]))
        );
    }

    #[test]
    fn aggregate_scope_is_repo_wide_if_any_cell_is_unresolved() {
        let cells = vec![
            SubprojectResolution::Resolved {
                subprojects: vec!["core".to_string()],
                inferred: false,
            },
            SubprojectResolution::Unresolved,
        ];
        assert_eq!(aggregate_scope(&cells), Scope::RepoWide);
    }

    #[test]
    fn aggregate_scope_is_repo_wide_if_any_cell_is_not_applicable() {
        assert_eq!(
            aggregate_scope(&[SubprojectResolution::NotApplicable]),
            Scope::RepoWide
        );
    }

    #[test]
    fn aggregate_scope_of_empty_cells_is_empty_areas() {
        assert_eq!(aggregate_scope(&[]), Scope::Areas(BTreeSet::new()));
    }

    #[test]
    fn squad_scope_groups_by_project_and_unions_resolved_subprojects() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-1", 0, 0, "auth", false);
        insert_bare_task(&store, "squad-1", 1, "daemon");
        insert_bare_cell(&store, "squad-1", 1, 0, None, None);
        insert_cell_subproject(&store, "squad-1", 1, 0, "store", true);

        let scope = store.squad_scope_by_project("squad-1").unwrap();
        assert_eq!(scope.len(), 2);
        assert_eq!(
            scope["core"],
            Scope::Areas(BTreeSet::from(["auth".to_string()]))
        );
        assert_eq!(
            scope["daemon"],
            Scope::Areas(BTreeSet::from(["store".to_string()]))
        );
    }

    #[test]
    fn squad_scope_is_repo_wide_for_a_project_with_an_unresolved_cell() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);

        let scope = store.squad_scope_by_project("squad-1").unwrap();
        assert_eq!(scope.get("core"), Some(&Scope::RepoWide));
    }

    #[test]
    fn review_scope_follows_direct_review_guardian_id_linkage() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_guardian(&store, "guardian-1");
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, Some("guardian-1"), None);
        insert_cell_subproject(&store, "squad-1", 0, 0, "auth", false);

        let scope = store.review_scope_by_project("guardian-1").unwrap();
        assert_eq!(
            scope.get("core"),
            Some(&Scope::Areas(BTreeSet::from(["auth".to_string()])))
        );
    }

    #[test]
    fn review_scope_falls_back_to_branch_match_when_no_direct_linkage() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_guardian(&store, "guardian-1");
        store
            .conn
            .execute(
                "INSERT INTO guardian_branches(guardian_id, position, branch, merge_status) VALUES(?,?,?,?)",
                params!["guardian-1", 0, "feature-x", "pending"],
            )
            .unwrap();
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, Some("feature-x"));
        insert_cell_subproject(&store, "squad-1", 0, 0, "auth", false);

        let scope = store.review_scope_by_project("guardian-1").unwrap();
        assert_eq!(
            scope.get("core"),
            Some(&Scope::Areas(BTreeSet::from(["auth".to_string()])))
        );
    }

    #[test]
    fn review_scope_pulls_cells_from_multiple_squads() {
        let store = Store::open_in_memory().unwrap();
        insert_bare_guardian(&store, "guardian-1");

        insert_bare_squad(&store, "squad-a", SquadState::Pending);
        insert_bare_task(&store, "squad-a", 0, "core");
        insert_bare_cell(&store, "squad-a", 0, 0, Some("guardian-1"), None);
        insert_cell_subproject(&store, "squad-a", 0, 0, "auth", false);

        insert_bare_squad(&store, "squad-b", SquadState::Pending);
        insert_bare_task(&store, "squad-b", 0, "core");
        insert_bare_cell(&store, "squad-b", 0, 0, Some("guardian-1"), None);
        insert_cell_subproject(&store, "squad-b", 0, 0, "billing", false);

        let scope = store.review_scope_by_project("guardian-1").unwrap();
        assert_eq!(
            scope.get("core"),
            Some(&Scope::Areas(BTreeSet::from([
                "auth".to_string(),
                "billing".to_string()
            ])))
        );
    }

    #[test]
    fn waypoint_scope_unions_squad_and_review_affected_entries_repo_wide_wins() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        insert_bare_guardian(&store, "guardian-1");
        insert_bare_squad(&store, "squad-a", SquadState::Pending);
        insert_bare_task(&store, "squad-a", 0, "core");
        insert_bare_cell(&store, "squad-a", 0, 0, Some("guardian-1"), None);
        insert_cell_subproject(&store, "squad-a", 0, 0, "auth", false);

        insert_bare_squad(&store, "squad-b", SquadState::Pending);
        insert_bare_task(&store, "squad-b", 0, "core");
        insert_bare_cell(&store, "squad-b", 0, 0, None, None);

        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-b",
                AffectedMode::Block,
            )
            .unwrap();

        let scope = store.waypoint_scope_by_project("waypoint-1").unwrap();
        assert_eq!(scope.get("core"), Some(&Scope::RepoWide));
    }

    #[test]
    fn waypoint_scope_unions_disjoint_areas_across_affected_entries() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        insert_bare_squad(&store, "squad-a", SquadState::Pending);
        insert_bare_task(&store, "squad-a", 0, "core");
        insert_bare_cell(&store, "squad-a", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-a", 0, 0, "auth", false);

        insert_bare_squad(&store, "squad-b", SquadState::Pending);
        insert_bare_task(&store, "squad-b", 0, "core");
        insert_bare_cell(&store, "squad-b", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-b", 0, 0, "billing", false);

        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-a",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-b",
                AffectedMode::Block,
            )
            .unwrap();

        let scope = store.waypoint_scope_by_project("waypoint-1").unwrap();
        assert_eq!(
            scope.get("core"),
            Some(&Scope::Areas(BTreeSet::from([
                "auth".to_string(),
                "billing".to_string()
            ])))
        );
    }

    fn insert_guardian_with_status(store: &Store, id: &str, status: &str) {
        let now = now_ms();
        store
            .conn
            .execute(
                "INSERT INTO guardians(id, name, base_branch, git_root, status, created_at_ms, updated_at_ms) VALUES(?,?,?,?,?,?,?)",
                params![id, "review", "main", "/tmp/repo", status, now, now],
            )
            .unwrap();
    }

    // ── parse_survey_reply ────────────────────────────────────────────────

    #[test]
    fn parse_survey_reply_matches_case_insensitively_and_trims_punctuation() {
        let verdict = parse_survey_reply(
            "  impacted:  YES.  \nrationale: \"touches the auth flow.\"  ",
            false,
        )
        .unwrap();
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, AffectedMode::Block);
        assert_eq!(verdict.rationale, "touches the auth flow");
    }

    #[test]
    fn parse_survey_reply_ignores_unrecognized_and_blank_lines() {
        let verdict = parse_survey_reply(
            "some preamble the model added\n\nIMPACTED: no\n\nRATIONALE: unrelated area\ntrailing junk",
            false,
        )
        .unwrap();
        assert!(!verdict.impacted);
        assert_eq!(verdict.rationale, "unrelated area");
    }

    #[test]
    fn parse_survey_reply_returns_none_when_impacted_line_missing() {
        assert!(parse_survey_reply("RATIONALE: no clear verdict given", false).is_none());
        assert!(parse_survey_reply("", false).is_none());
        assert!(parse_survey_reply("IMPACTED: maybe", false).is_none());
    }

    #[test]
    fn parse_survey_reply_defaults_rationale_when_model_omits_it() {
        let verdict = parse_survey_reply("IMPACTED: yes", false).unwrap();
        assert_eq!(verdict.rationale, "model gave no rationale");
    }

    #[test]
    fn parse_survey_reply_advisory_mode_only_applies_when_allowed_and_impacted() {
        // allow_advisory=true, impacted=yes, MODE: advisory -> Advisory.
        let verdict =
            parse_survey_reply("IMPACTED: yes\nMODE: advisory\nRATIONALE: fyi only", true).unwrap();
        assert_eq!(verdict.mode, AffectedMode::Advisory);

        // allow_advisory=true, impacted=yes, MODE: block -> Block.
        let verdict = parse_survey_reply("IMPACTED: yes\nMODE: block\nRATIONALE: r", true).unwrap();
        assert_eq!(verdict.mode, AffectedMode::Block);

        // allow_advisory=true but impacted=no -> always Block regardless of MODE.
        let verdict =
            parse_survey_reply("IMPACTED: no\nMODE: advisory\nRATIONALE: r", true).unwrap();
        assert_eq!(verdict.mode, AffectedMode::Block);

        // allow_advisory=false -> a MODE line is ignored entirely, always Block.
        let verdict =
            parse_survey_reply("IMPACTED: yes\nMODE: advisory\nRATIONALE: r", false).unwrap();
        assert_eq!(verdict.mode, AffectedMode::Block);
    }

    // ── resolve_survey_verdict (fail-closed + advisory de-escalation) ──────

    #[test]
    fn resolve_survey_verdict_fails_closed_on_call_error() {
        let verdict = resolve_survey_verdict(Err("connection refused".to_string()), false);
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, AffectedMode::Block);
        assert!(verdict.rationale.contains("connection refused"));
    }

    #[test]
    fn resolve_survey_verdict_fails_closed_on_unparseable_reply() {
        let verdict =
            resolve_survey_verdict(Ok("the model rambled without a verdict".to_string()), false);
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, AffectedMode::Block);
        assert!(verdict.rationale.contains("unparseable"));
    }

    #[test]
    fn resolve_survey_verdict_applies_advisory_deescalation_when_allowed() {
        let verdict = resolve_survey_verdict(
            Ok(
                "IMPACTED: yes\nMODE: advisory\nRATIONALE: just keep this squad informed"
                    .to_string(),
            ),
            true,
        );
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, AffectedMode::Advisory);
    }

    #[test]
    fn resolve_survey_verdict_keeps_block_when_advisory_not_allowed() {
        let verdict = resolve_survey_verdict(
            Ok("IMPACTED: yes\nMODE: advisory\nRATIONALE: r".to_string()),
            false,
        );
        assert!(verdict.impacted);
        assert_eq!(verdict.mode, AffectedMode::Block);
    }

    #[test]
    fn resolve_survey_verdict_passes_through_a_clean_not_impacted_reply() {
        let verdict = resolve_survey_verdict(
            Ok("IMPACTED: no\nRATIONALE: different area entirely".to_string()),
            true,
        );
        assert!(!verdict.impacted);
        assert_eq!(verdict.mode, AffectedMode::Block);
    }

    // ── waypoint_survey_candidates (matching-model population) ─────────────

    #[test]
    fn survey_candidates_include_overlapping_area_and_exclude_sibling_area() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        // Seed the waypoint's scope with one affected entry in project "core",
        // area "auth" -- standing in for whatever adds the initial affected
        // entry a waypoint is declared against.
        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed", 0, 0, "auth", false);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();

        // Same project, same area -> must be surveyed.
        insert_bare_squad(&store, "squad-match", SquadState::Pending);
        insert_bare_task(&store, "squad-match", 0, "core");
        insert_bare_cell(&store, "squad-match", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-match", 0, 0, "auth", false);

        // Same project, unrelated sibling area -> must never be surveyed.
        insert_bare_squad(&store, "squad-sibling", SquadState::Pending);
        insert_bare_task(&store, "squad-sibling", 0, "core");
        insert_bare_cell(&store, "squad-sibling", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-sibling", 0, 0, "billing", false);

        // Different project entirely, same area name -> must never be
        // surveyed (a project match is required before area overlap even
        // applies).
        insert_bare_squad(&store, "squad-other-project", SquadState::Pending);
        insert_bare_task(&store, "squad-other-project", 0, "other");
        insert_bare_cell(&store, "squad-other-project", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-other-project", 0, 0, "auth", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert_eq!(
            candidates.len(),
            1,
            "unrelated-area/project candidates must gain zero affected/survey state: {candidates:?}"
        );
        assert_eq!(candidates[0].kind, WaypointEntryKind::Squad);
        assert_eq!(candidates[0].entry_id, "squad-match");
    }

    #[test]
    fn survey_candidates_cover_two_explicit_named_areas() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        // Seed the waypoint against both "auth" and "billing" via two
        // separately-scoped affected entries.
        insert_bare_squad(&store, "squad-seed-auth", SquadState::Pending);
        insert_bare_task(&store, "squad-seed-auth", 0, "core");
        insert_bare_cell(&store, "squad-seed-auth", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed-auth", 0, 0, "auth", false);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed-auth",
                AffectedMode::Block,
            )
            .unwrap();

        insert_bare_squad(&store, "squad-seed-billing", SquadState::Pending);
        insert_bare_task(&store, "squad-seed-billing", 0, "core");
        insert_bare_cell(&store, "squad-seed-billing", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed-billing", 0, 0, "billing", false);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed-billing",
                AffectedMode::Block,
            )
            .unwrap();

        for (id, area) in [("squad-auth", "auth"), ("squad-billing", "billing")] {
            insert_bare_squad(&store, id, SquadState::Pending);
            insert_bare_task(&store, id, 0, "core");
            insert_bare_cell(&store, id, 0, 0, None, None);
            insert_cell_subproject(&store, id, 0, 0, area, false);
        }
        // A third, unrelated area -- must stay excluded even though the
        // waypoint already spans two areas.
        insert_bare_squad(&store, "squad-shipping", SquadState::Pending);
        insert_bare_task(&store, "squad-shipping", 0, "core");
        insert_bare_cell(&store, "squad-shipping", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-shipping", 0, 0, "shipping", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        let mut ids: Vec<&str> = candidates.iter().map(|c| c.entry_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["squad-auth", "squad-billing"]);
    }

    #[test]
    fn survey_candidates_fall_back_to_repo_wide_within_scoped_project_only() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        // Seed affected entry whose cell never got a subproject resolution --
        // this project's scope can't be safely narrowed, so it goes
        // repo-wide (conservative fallback), per Phase 0.
        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();
        assert_eq!(
            store
                .waypoint_scope_by_project("waypoint-1")
                .unwrap()
                .get("core"),
            Some(&Scope::RepoWide)
        );

        // Same project, a totally unrelated area -- included anyway because
        // the project's scope is repo-wide.
        insert_bare_squad(&store, "squad-same-project", SquadState::Pending);
        insert_bare_task(&store, "squad-same-project", 0, "core");
        insert_bare_cell(&store, "squad-same-project", 0, 0, None, None);
        insert_cell_subproject(
            &store,
            "squad-same-project",
            0,
            0,
            "wholly-unrelated",
            false,
        );

        // A different project (a different repo/root) is not swept in by
        // another project's repo-wide fallback.
        insert_bare_squad(&store, "squad-cross-project", SquadState::Pending);
        insert_bare_task(&store, "squad-cross-project", 0, "other");
        insert_bare_cell(&store, "squad-cross-project", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-cross-project", 0, 0, "auth", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].entry_id, "squad-same-project");
    }

    #[test]
    fn survey_candidates_exclude_terminal_and_already_affecteded_entries() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed", 0, 0, "auth", false);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();

        // Overlapping scope but already terminal -> excluded.
        insert_bare_squad(&store, "squad-done", SquadState::Done);
        insert_bare_task(&store, "squad-done", 0, "core");
        insert_bare_cell(&store, "squad-done", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-done", 0, 0, "auth", false);

        // Overlapping scope but already an explicit affected entry -> excluded
        // (it's already been surveyed/tracked, not a fresh candidate).
        insert_bare_squad(&store, "squad-already-affecteded", SquadState::Pending);
        insert_bare_task(&store, "squad-already-affecteded", 0, "core");
        insert_bare_cell(&store, "squad-already-affecteded", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-already-affecteded", 0, 0, "auth", false);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-already-affecteded",
                AffectedMode::Block,
            )
            .unwrap();

        // A terminal review, and a fresh matching review -- reviews follow
        // the identical exclusion rules as squads.
        insert_guardian_with_status(&store, "guardian-merged", "merged");
        insert_bare_squad(&store, "squad-for-merged-review", SquadState::Pending);
        insert_bare_task(&store, "squad-for-merged-review", 0, "core");
        insert_bare_cell(
            &store,
            "squad-for-merged-review",
            0,
            0,
            Some("guardian-merged"),
            None,
        );
        insert_cell_subproject(&store, "squad-for-merged-review", 0, 0, "auth", false);

        insert_guardian_with_status(&store, "guardian-open", "collecting");
        insert_bare_squad(&store, "squad-for-open-review", SquadState::Pending);
        insert_bare_task(&store, "squad-for-open-review", 0, "core");
        insert_bare_cell(
            &store,
            "squad-for-open-review",
            0,
            0,
            Some("guardian-open"),
            None,
        );
        insert_cell_subproject(&store, "squad-for-open-review", 0, 0, "auth", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        let squad_ids: Vec<&str> = candidates
            .iter()
            .filter(|c| c.kind == WaypointEntryKind::Squad)
            .map(|c| c.entry_id.as_str())
            .collect();
        let review_ids: Vec<&str> = candidates
            .iter()
            .filter(|c| c.kind == WaypointEntryKind::Review)
            .map(|c| c.entry_id.as_str())
            .collect();
        assert_eq!(
            squad_ids,
            vec!["squad-for-merged-review", "squad-for-open-review"],
            "the squads backing both reviews are themselves fresh, non-terminal, un-affecteded candidates too"
        );
        assert_eq!(review_ids, vec!["guardian-open"]);
    }

    #[test]
    fn survey_candidates_empty_until_the_waypoint_has_a_seeded_affected_scope() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-1", 0, 0, "auth", false);

        // A waypoint with an empty affected has no aggregate scope to overlap
        // against, so it must select nothing rather than guess.
        assert!(
            store
                .waypoint_survey_candidates("waypoint-1")
                .unwrap()
                .is_empty()
        );
    }

    /// Marks a cell as if `run_cell_worker`'s `is_waypoint_halted()` branch
    /// had just run: DB `state='running'` (what `record_cell_result`
    /// persists for a waypoint-halted `RunnerResult`) plus
    /// `waypoint_halted_at_ms` set via [`Store::mark_cell_waypoint_halted`].
    fn halt_cell(store: &Store, squad_id: &str, task_idx: i64, idx: i64) {
        store
            .conn
            .execute(
                "UPDATE cells SET state='running' WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, idx],
            )
            .unwrap();
        store
            .mark_cell_waypoint_halted(squad_id, task_idx, idx)
            .unwrap();
    }

    fn handle(store: Store) -> Arc<StoreMutex> {
        Arc::new(StoreMutex::new(store))
    }

    #[test]
    fn resume_sweep_leaves_a_halted_cell_alone_while_the_waypoint_still_blocks() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        // A squad's hold lasts while the waypoint's own work is unfinished, so
        // give it an unfinished goal to be held against.
        insert_bare_squad(&store, "squad-goal", SquadState::Pending);
        store
            .add_roster_entry("waypoint-1", WaypointEntryKind::Squad, "squad-goal", None)
            .unwrap();
        insert_bare_squad(&store, "squad-1", SquadState::Running);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        halt_cell(&store, "squad-1", 0, 0);

        let handle = handle(store);
        let cancellations = Cancellations::new();
        run_pending_waypoint_resumes(&handle, &cancellations);

        let store = handle.lock();
        assert_eq!(
            store.cell_state("squad-1", 0, 0).unwrap(),
            Some(NodeState::Running),
            "still blocked -- the halted cell must not be resumed"
        );
        assert_eq!(store.squad_state("squad-1").unwrap(), SquadState::Running);
    }

    #[test]
    fn resume_sweep_frees_a_halted_cell_and_squad_once_the_waypoint_closes() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Running);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        halt_cell(&store, "squad-1", 0, 0);
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);
        // No worker registered for "squad-1" -- simulates the worker thread
        // having already exited (`run_cell_worker` returned once its cell
        // halted), so the sweep must also nudge the squad's own row back to
        // `Pending` for a fresh `scheduler::tick` to pick it up.
        let cancellations = Cancellations::new();
        run_pending_waypoint_resumes(&handle, &cancellations);

        let store = handle.lock();
        assert_eq!(
            store.cell_state("squad-1", 0, 0).unwrap(),
            Some(NodeState::Pending),
            "waypoint closed -- the halted cell must resume"
        );
        assert_eq!(store.squad_state("squad-1").unwrap(), SquadState::Pending);
    }

    #[test]
    fn resume_sweep_leaves_the_squads_own_row_alone_while_its_worker_is_still_alive() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Running);
        insert_bare_task(&store, "squad-1", 0, "core");
        insert_bare_cell(&store, "squad-1", 0, 0, None, None);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        halt_cell(&store, "squad-1", 0, 0);
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);
        let cancellations = Cancellations::new();
        // Still-registered token: the squad's worker thread hasn't exited,
        // so it will notice the cell's DB state flip via the same
        // reclaimed-detached reconciliation `resume_detached_cell` relies
        // on -- the sweep must not also stomp the squad's own row.
        let _token = cancellations.register("squad-1");
        run_pending_waypoint_resumes(&handle, &cancellations);

        let store = handle.lock();
        assert_eq!(
            store.cell_state("squad-1", 0, 0).unwrap(),
            Some(NodeState::Pending),
            "waypoint closed -- the halted cell must still resume"
        );
        assert_eq!(
            store.squad_state("squad-1").unwrap(),
            SquadState::Running,
            "worker still alive -- its own row must not be touched"
        );
    }

    // ── run_pending_deliveries (Phase 4) ─────────────────────────────────

    /// Always returns a `"done"` result -- delivery only cares about
    /// `start_feedback`'s own synchronous `Reply`, not what its spawned
    /// background thread (which calls `run_feedback` with this runner) goes
    /// on to do with a fake, non-existent worktree.
    struct DeliveryTestRunner;

    impl crate::runner::Runner for DeliveryTestRunner {
        fn run(&self, _spec: &crate::runner::RunnerSpec) -> crate::runner::RunnerResult {
            crate::runner::RunnerResult {
                status: "done".to_string(),
                ..crate::runner::RunnerResult::failure("unused")
            }
        }
    }

    /// Adds an enabled branch with a review branch/worktree already recorded
    /// -- the readiness bar both [`topmost_ready_branch`] and
    /// `guardian_merge::start_feedback` itself enforce -- and returns its id.
    fn open_review_branch(store: &Store, guardian_id: &str, branch: &str) -> String {
        let position = store.add_guardian_branch(guardian_id, branch).unwrap();
        let branch_id = store.get_guardian(guardian_id).unwrap().branches
            [usize::try_from(position).unwrap()]
        .id
        .clone();
        store
            .set_branch_review(
                guardian_id,
                &branch_id,
                "review-branch",
                "/tmp/fake-worktree",
            )
            .unwrap();
        branch_id
    }

    #[test]
    fn run_pending_deliveries_marks_an_impacted_review_entry_delivered() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        open_review_branch(&store, "guardian-1", "feature-x");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Block,
            )
            .unwrap();

        let handle = handle(store);
        let runner: Arc<dyn crate::runner::Runner> = Arc::new(DeliveryTestRunner);
        run_pending_deliveries(&handle, &runner, &crate::cancel::Cancellations::new());

        let store = handle.lock();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].delivery_status, DeliveryStatus::Delivered);
    }

    #[test]
    fn run_pending_deliveries_skips_a_not_impacted_review_entry() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        open_review_branch(&store, "guardian-1", "feature-x");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                &SurveyVerdict {
                    impacted: false,
                    mode: AffectedMode::Block,
                    rationale: "no overlap".to_string(),
                },
            )
            .unwrap();

        let handle = handle(store);
        let runner: Arc<dyn crate::runner::Runner> = Arc::new(DeliveryTestRunner);
        run_pending_deliveries(&handle, &runner, &crate::cancel::Cancellations::new());

        let store = handle.lock();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert_eq!(
            entries[0].delivery_status,
            DeliveryStatus::Undelivered,
            "not_impacted must never be delivered to"
        );
    }

    #[test]
    fn run_pending_deliveries_marks_a_done_but_unreviewed_squad_via_restack() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Done);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();

        let handle = handle(store);
        let runner: Arc<dyn crate::runner::Runner> = Arc::new(DeliveryTestRunner);
        run_pending_deliveries(&handle, &runner, &crate::cancel::Cancellations::new());

        let store = handle.lock();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert_eq!(entries[0].delivery_status, DeliveryStatus::ViaRestack);
    }

    #[test]
    fn run_pending_deliveries_leaves_a_still_running_squad_entry_untouched() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Running);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();

        let handle = handle(store);
        let runner: Arc<dyn crate::runner::Runner> = Arc::new(DeliveryTestRunner);
        run_pending_deliveries(&handle, &runner, &crate::cancel::Cancellations::new());

        let store = handle.lock();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert_eq!(
            entries[0].delivery_status,
            DeliveryStatus::Undelivered,
            "still-running squad has nothing to do yet -- it is reached later, either \
             directly (once non-terminal) or via-restack (once terminal)"
        );
    }

    // ── waypoint_survey_candidates population filter (Phase 4 addition) ──

    #[test]
    fn survey_candidates_exclude_cancelled_squads_by_construction() {
        // RAL-400 Phase 4 AC: fully-done/cancelled squads are excluded from
        // the classification population *by construction* -- this covers the
        // `Cancelled` terminal variant specifically, alongside the existing
        // `survey_candidates_exclude_terminal_and_already_affecteded_entries`
        // test's coverage of `Done`.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed", 0, 0, "auth", false);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();

        // Overlapping scope but cancelled -> excluded.
        insert_bare_squad(&store, "squad-cancelled", SquadState::Cancelled);
        insert_bare_task(&store, "squad-cancelled", 0, "core");
        insert_bare_cell(&store, "squad-cancelled", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-cancelled", 0, 0, "auth", false);

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert!(
            candidates.is_empty(),
            "a cancelled squad must never surface as a survey candidate: {candidates:?}"
        );
    }

    // ── auto-enrollment + surveyability (submit-time gating gap) ─────────

    #[test]
    fn an_explicitly_declared_affected_entry_is_never_a_survey_candidate() {
        // The classifier must never get the chance to downgrade a human's
        // declaration to `not_impacted` and release a gate they asked for.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert!(
            !candidates
                .iter()
                .any(|c| c.entry_id == "squad-seed" && c.kind == WaypointEntryKind::Squad),
            "an explicit (non-auto-enrolled) entry must stay out of the survey \
             population even with a NULL verdict: {candidates:?}"
        );
    }

    #[test]
    fn an_auto_enrolled_entry_stays_surveyable_until_it_has_a_verdict() {
        // Submit-time enrollment writes a blocking, unsurveyed row on
        // purpose. If such a row were treated as "already affecteded" it would
        // never be surveyed, and its NULL verdict would block the squad
        // forever -- so it must remain a candidate until a verdict lands, and
        // drop out once one does.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();

        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert!(
            candidates
                .iter()
                .any(|c| c.entry_id == "squad-seed" && c.kind == WaypointEntryKind::Squad),
            "an auto-enrolled entry with no verdict must still be surveyable: {candidates:?}"
        );

        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                &SurveyVerdict {
                    impacted: false,
                    mode: AffectedMode::Block,
                    rationale: "unrelated".to_string(),
                },
            )
            .unwrap();
        let candidates = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert!(
            !candidates.iter().any(|c| c.entry_id == "squad-seed"),
            "a surveyed entry must not be re-surveyed: {candidates:?}"
        );
    }

    #[test]
    fn submitting_a_squad_under_an_open_waypoint_blocks_it_before_any_survey_runs() {
        // The gate is only read when a squad is claimed, and the survey sweep
        // runs up to WAYPOINT_SURVEY_INTERVAL later -- so without submit-time
        // enrollment a squad submitted under an open waypoint starts
        // unguarded, and a short one finishes (going terminal, hence
        // permanently unsurveyable) before the waypoint ever sees it.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        // A squad's hold lasts while the waypoint's own work is unfinished, so
        // give it an unfinished goal to be held against.
        insert_bare_squad(&store, "squad-goal", SquadState::Pending);
        store
            .add_roster_entry("waypoint-1", WaypointEntryKind::Squad, "squad-goal", None)
            .unwrap();
        // Seed entry so the waypoint has a scope to overlap against.
        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();

        insert_bare_squad(&store, "squad-new", SquadState::Pending);
        insert_bare_task(&store, "squad-new", 0, "core");
        insert_bare_cell(&store, "squad-new", 0, 0, None, None);
        assert_eq!(
            store.squad_block_gating_waypoint("squad-new").unwrap(),
            None,
            "precondition: nothing gates the squad before it is enrolled"
        );

        let enrolled = store
            .enroll_new_squad_in_open_waypoints("squad-new")
            .unwrap();
        assert_eq!(enrolled, vec!["waypoint-1".to_string()]);
        assert_eq!(
            store
                .squad_block_gating_waypoint("squad-new")
                .unwrap()
                .as_deref(),
            Some("waypoint-1"),
            "an enrolled squad must be gated immediately, on its NULL verdict"
        );
    }

    #[test]
    fn submit_time_enrollment_skips_a_waypoint_whose_scope_does_not_overlap() {
        // Enrollment must not delay work a waypoint provably cannot affect:
        // two confidently-Resolved, non-overlapping subproject sets.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-seed", 0, 0, "auth", false);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();

        insert_bare_squad(&store, "squad-elsewhere", SquadState::Pending);
        insert_bare_task(&store, "squad-elsewhere", 0, "core");
        insert_bare_cell(&store, "squad-elsewhere", 0, 0, None, None);
        insert_cell_subproject(&store, "squad-elsewhere", 0, 0, "billing", false);

        let enrolled = store
            .enroll_new_squad_in_open_waypoints("squad-elsewhere")
            .unwrap();
        assert!(
            enrolled.is_empty(),
            "a non-overlapping squad must not be enrolled or delayed: {enrolled:?}"
        );
        assert_eq!(
            store
                .squad_block_gating_waypoint("squad-elsewhere")
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_survey_description_names_the_work_not_just_the_id() {
        // The classifier cannot judge relevance from an opaque id; the
        // description is the only signal it gets, so it must carry the task
        // name and the cell's own prompt/command text.
        let store = Store::open_in_memory().unwrap();
        insert_bare_squad(&store, "squad-1", SquadState::Pending);
        store
            .conn
            .execute(
                "INSERT INTO tasks(squad_id, idx, name, project, state)
                 VALUES('squad-1',0,'rewrite-greet-callers','core','pending')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, prompt, cwd)
                 VALUES('squad-1',0,0,'s0-0','claude-code','pending','update every caller of greet()','/repo/src')",
                [],
            )
            .unwrap();

        let described = store.describe_candidate_for_survey(WaypointEntryKind::Squad, "squad-1");
        assert!(
            described.contains("rewrite-greet-callers"),
            "description must name the task: {described}"
        );
        assert!(
            described.contains("update every caller of greet()"),
            "description must carry the cell's prompt: {described}"
        );
        assert!(
            described.contains("/repo/src"),
            "description must carry the cell's cwd: {described}"
        );
    }

    #[test]
    fn a_survey_description_degrades_to_the_bare_id_for_a_missing_candidate() {
        // A candidate whose rows vanished mid-sweep must still classify
        // (fail-closed) rather than abort the whole sweep.
        let store = Store::open_in_memory().unwrap();
        let described = store.describe_candidate_for_survey(WaypointEntryKind::Squad, "squad-gone");
        assert_eq!(described, "squad squad-gone");
    }

    #[test]
    fn a_long_cell_prompt_is_truncated_and_flattened_in_the_description() {
        let long = "x".repeat(SURVEY_DESCRIPTION_FIELD_CHARS * 3);
        let out = truncate_for_survey(&long);
        assert!(out.ends_with("..."));
        assert_eq!(out.chars().count(), SURVEY_DESCRIPTION_FIELD_CHARS + 3);
        assert_eq!(
            truncate_for_survey("line one\nIMPACTED: yes\nline three"),
            "line one IMPACTED: yes line three",
            "newlines must collapse so prompt text cannot forge reply lines"
        );
    }

    // ── stand-down notices (close-time, watcher-addressed only) ──────────

    /// A `Runner` that fails the test if anything dispatches it. Stand-down
    /// must never reach a runner: the whole point is that closing a waypoint
    /// notifies people without spending an agent turn.
    struct NoDispatchRunner;

    impl crate::runner::Runner for NoDispatchRunner {
        fn run(&self, _spec: &crate::runner::RunnerSpec) -> crate::runner::RunnerResult {
            panic!("stand-down must not dispatch an agent");
        }
    }

    #[test]
    fn a_review_stand_down_notifies_watchers_without_dispatching_an_agent() {
        // This used to go through `guardian_merge::start_feedback`, which runs
        // the resolver agent and churns the review through merging/actioning
        // -- a billed turn to deliver "no further action is needed".
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        open_review_branch(&store, "guardian-1", "feature-x");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);
        // Registered but never used: a dispatch would panic.
        let _never: Arc<dyn Runner> = Arc::new(NoDispatchRunner);
        run_pending_stand_down_notices(&handle);

        let store = handle.lock();
        assert!(
            store.list_affected_entries("waypoint-1").unwrap()[0]
                .stand_down_at_ms
                .is_some(),
            "the entry must be marked stood-down"
        );
        let mail = mail(&store);
        assert_eq!(mail.len(), 1, "exactly one notice: {mail:?}");
        assert_eq!(
            mail[0].entity_uri.as_deref(),
            Some("guardian:guardian-1"),
            "addressed to the review, so its watchers match"
        );
        assert!(mail[0].message.contains("no further action is needed"));
    }

    #[test]
    fn a_squad_stand_down_is_addressed_to_each_cell_the_waypoint_advised() {
        // A watch matches when the watched entity covers the message's, parent
        // to child -- so a lone `squad:` row never reaches someone watching one
        // specific cell, which is exactly the person closest to the advised
        // work.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cwd(&store, "squad-1", "/repo");
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, prompt, cwd)
                 VALUES('squad-1',0,1,'s0-1','claude-code','running','later','/repo')",
                [],
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        let bearing = seed_bearing(&store, "waypoint-1", "salute replaces greet");
        store
            .queue_advisory_bearing_injections("waypoint-1", &bearing)
            .unwrap();
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);
        run_pending_stand_down_notices(&handle);

        let store = handle.lock();
        let uris: BTreeSet<String> = mail(&store)
            .into_iter()
            .filter_map(|m| m.entity_uri)
            .collect();
        assert!(
            uris.contains("cell:squad-1:0:0") && uris.contains("cell:squad-1:0:1"),
            "each advised cell must be addressed directly: {uris:?}"
        );
        assert!(
            !uris.contains("squad:squad-1"),
            "the squad-level row is the fallback only, not an extra copy: {uris:?}"
        );
    }

    #[test]
    fn a_squad_the_waypoint_never_reached_a_cell_of_falls_back_to_one_row() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cwd(&store, "squad-1", "/repo");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);
        run_pending_stand_down_notices(&handle);

        let store = handle.lock();
        let sent = mail(&store);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].entity_uri.as_deref(), Some("squad:squad-1"));
    }

    // ── scope project keying (registered project, not cwd basename) ──────

    #[test]
    fn scope_keys_on_the_registered_project_not_the_cwd_basename() {
        // `TaskView::project` degrades to the last path component of a cell's
        // cwd when the task declares none, so keying scope on it partitioned
        // one registered repository by subdirectory: work in `repo/core` and
        // work in `repo` landed under different "projects" and could never
        // match, hiding genuinely impacted work from its waypoint.
        let store = Store::open_in_memory().unwrap();
        store.register_project("mono", "", "/repo", "git").unwrap();

        insert_bare_squad(&store, "squad-sub", SquadState::Pending);
        insert_bare_task(&store, "squad-sub", 0, "core");
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, cwd)
                 VALUES('squad-sub',0,0,'s0-0','claude-code','pending','/repo/core')",
                [],
            )
            .unwrap();

        insert_bare_squad(&store, "squad-root", SquadState::Pending);
        insert_bare_task(&store, "squad-root", 0, "repo");
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, cwd)
                 VALUES('squad-root',0,0,'s0-0','claude-code','pending','/repo')",
                [],
            )
            .unwrap();

        let sub = store.squad_scope_by_project("squad-sub").unwrap();
        let root = store.squad_scope_by_project("squad-root").unwrap();
        assert!(
            sub.contains_key("mono") && root.contains_key("mono"),
            "both must key on the registered project: {sub:?} / {root:?}"
        );
        assert!(
            !sub.contains_key("core"),
            "the cwd basename must not become a project key: {sub:?}"
        );
    }

    #[test]
    fn a_waypoint_matches_impacted_work_elsewhere_in_the_same_registered_repo() {
        // The end-to-end shape of the bug above: a waypoint affecteded from a
        // subdirectory must still enrol genuinely impacted work submitted from
        // the repository root.
        let store = Store::open_in_memory().unwrap();
        store.register_project("mono", "", "/repo", "git").unwrap();
        open_waypoint(&store, "waypoint-1");

        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, cwd)
                 VALUES('squad-seed',0,0,'s0-0','claude-code','pending','/repo/core')",
                [],
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();

        insert_bare_squad(&store, "squad-root", SquadState::Pending);
        insert_bare_task(&store, "squad-root", 0, "repo");
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, cwd)
                 VALUES('squad-root',0,0,'s0-0','claude-code','pending','/repo')",
                [],
            )
            .unwrap();

        let enrolled = store
            .enroll_new_squad_in_open_waypoints("squad-root")
            .unwrap();
        assert_eq!(
            enrolled,
            vec!["waypoint-1".to_string()],
            "impacted work elsewhere in the same repo must be enrolled"
        );
    }

    // ── blocked / advised notifications ──────────────────────────────────

    fn mail(store: &Store) -> Vec<crate::mailbox::MailboxMessageView> {
        store
            .mailbox_messages_for_client("client", false, None)
            .unwrap()
    }

    /// Drive one survey to a chosen verdict through a canned runner, so the
    /// notification each verdict produces can be asserted without a live
    /// model.
    fn survey_with_reply(reply: &str) -> Vec<crate::mailbox::MailboxMessageView> {
        let store = Store::open_in_memory().unwrap();
        store
            .create_waypoint(
                "waypoint-1",
                Some("greet rename"),
                "greet() is being renamed",
                Some("claude-code"),
                Some("sonnet"),
                true,
            )
            .unwrap();
        insert_squad_with_cwd(&store, "squad-1", "/repo");
        let handle = handle(store);
        let as_runner: Arc<dyn Runner> = SurveyTestRunner::new(reply);
        survey_candidate(
            &handle,
            "waypoint-1",
            &SurveyCandidate {
                kind: WaypointEntryKind::Squad,
                entry_id: "squad-1".to_string(),
            },
            &Cancellations::new(),
            &as_runner,
        )
        .unwrap();
        let guard = handle.lock();
        mail(&guard)
    }

    #[test]
    fn a_block_verdict_notifies_that_the_work_is_held_with_a_way_out() {
        let messages =
            survey_with_reply("IMPACTED: yes\nMODE: block\nRATIONALE: touches src/app.py");
        let blocked: Vec<_> = messages
            .iter()
            .filter(|m| m.event_kind.as_deref() == Some("waypoint_blocked"))
            .collect();
        assert_eq!(blocked.len(), 1, "one blocked notice: {messages:?}");
        assert!(
            blocked[0].message.contains("touches src/app.py"),
            "the notice must say why it is held: {}",
            blocked[0].message
        );
        assert!(
            blocked[0].message.contains("advisory"),
            "a blocked state must name a way out (RAL-502): {}",
            blocked[0].message
        );
    }

    #[test]
    fn an_advisory_verdict_notifies_without_claiming_the_work_is_held() {
        let messages =
            survey_with_reply("IMPACTED: yes\nMODE: advisory\nRATIONALE: only reads the helper");
        let advised: Vec<_> = messages
            .iter()
            .filter(|m| m.event_kind.as_deref() == Some("waypoint_advised"))
            .collect();
        assert_eq!(advised.len(), 1, "one advised notice: {messages:?}");
        assert!(
            advised[0].message.contains("not held"),
            "advisory must not read as blocking: {}",
            advised[0].message
        );
        assert!(
            advised[0].message.contains("inspect the current state"),
            "must carry the don't-assume-it's-landed contract: {}",
            advised[0].message
        );
        assert!(
            !messages
                .iter()
                .any(|m| m.event_kind.as_deref() == Some("waypoint_blocked")),
            "an advisory verdict must never emit a blocked notice"
        );
    }

    #[test]
    fn a_not_impacted_verdict_notifies_that_the_hold_is_lifted() {
        // Someone told their squad was gated at submit needs to hear when the
        // survey clears it, or the first notice reads as a dead end.
        let messages = survey_with_reply("IMPACTED: no\nRATIONALE: docs only");
        assert!(
            messages
                .iter()
                .any(|m| m.message.contains("no longer holds")),
            "a release must be announced: {messages:?}"
        );
        assert!(
            !messages
                .iter()
                .any(|m| m.event_kind.as_deref() == Some("waypoint_blocked")),
            "a cleared entry must not be reported as blocked"
        );
    }

    #[test]
    fn an_open_block_mode_waypoint_holds_a_reviews_approval() {
        // RAL-400 defines block mode for a review as holding approval until
        // the waypoint closes. Nothing consulted that before, so a blocked
        // review could be approved and merged anyway -- the silent bypass the
        // ticket's Risks section warns about.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        let guardian_id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_status(
                &guardian_id,
                crate::guardian::GuardianStatus::InReview,
                None,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                &guardian_id,
                AffectedMode::Block,
            )
            .unwrap();

        let err = store.approve_guardian(&guardian_id).unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidTransition(ref m) if m.contains("waypoint-1")),
            "approval must be refused and name the waypoint, got {err:?}"
        );

        // De-escalating releases it, without closing the waypoint.
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                &guardian_id,
                AffectedMode::Advisory,
            )
            .unwrap();
        assert_eq!(
            store.approve_guardian(&guardian_id).unwrap(),
            crate::guardian::GuardianStatus::Approved,
            "an advisory entry must never hold approval"
        );
    }

    #[test]
    fn closing_a_waypoint_releases_a_held_reviews_approval() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        let guardian_id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_status(
                &guardian_id,
                crate::guardian::GuardianStatus::InReview,
                None,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                &guardian_id,
                AffectedMode::Block,
            )
            .unwrap();
        assert!(store.approve_guardian(&guardian_id).is_err());
        assert!(store.close_waypoint("waypoint-1").unwrap());
        assert_eq!(
            store.approve_guardian(&guardian_id).unwrap(),
            crate::guardian::GuardianStatus::Approved
        );
    }

    #[test]
    fn an_unsurveyed_review_entry_holds_approval_fail_closed() {
        // A NULL verdict is not a cleared one: the same fail-closed reading
        // the squad gate uses must apply to reviews, or an entry awaiting its
        // survey would be approvable in the window before it runs.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        let guardian_id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_status(
                &guardian_id,
                crate::guardian::GuardianStatus::InReview,
                None,
            )
            .unwrap();
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                &guardian_id,
                AffectedMode::Block,
            )
            .unwrap();
        assert_eq!(
            store
                .review_block_gating_waypoint(&guardian_id)
                .unwrap()
                .as_deref(),
            Some("waypoint-1")
        );
        assert!(store.approve_guardian(&guardian_id).is_err());

        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Review,
                &guardian_id,
                &SurveyVerdict {
                    impacted: false,
                    mode: AffectedMode::Block,
                    rationale: "unrelated".to_string(),
                },
            )
            .unwrap();
        assert!(
            store.approve_guardian(&guardian_id).is_ok(),
            "a not-impacted verdict must release the approval hold"
        );
    }

    // ── consolidated effect feed (waypoint_deliveries) ───────────────────

    /// Record a waypoint-scoped Cartographer row as some other subsystem
    /// would, so the feed can be tested for the cross-source coverage that is
    /// the whole point of it.
    fn log_waypoint_effect(
        store: &Store,
        source: &'static str,
        message: &'static str,
        squad_id: Option<&str>,
        cell_id: Option<&str>,
        payload: serde_json::Value,
    ) {
        store
            .cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source,
                message,
                scope: Some("waypoint"),
                squad_id,
                guardian_id: None,
                cell_id,
                task: None,
                log_path: None,
                payload,
                admin_only: false,
            })
            .unwrap();
    }

    #[test]
    fn the_effect_feed_includes_rows_from_every_subsystem_not_just_waypoints() {
        // This feed previously filtered `source = "waypoints"`, which dropped
        // every effect the scheduler and submit path record -- the cell halts,
        // the advisory deliveries, the submit-time gating. It showed the
        // survey's decisions and nothing the waypoint actually did, while
        // looking complete.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");

        log_waypoint_effect(
            &store,
            "waypoints",
            "survey verdict",
            Some("squad-1"),
            None,
            serde_json::json!({"waypoint_id": "waypoint-1"}),
        );
        log_waypoint_effect(
            &store,
            "scheduler",
            "cell halted by waypoint block",
            Some("squad-1"),
            Some("cell-0"),
            serde_json::json!({"waypoint_id": "waypoint-1"}),
        );
        log_waypoint_effect(
            &store,
            "submit",
            "squad enrolled pending survey",
            Some("squad-2"),
            None,
            // Array shape: one submit can enrol a squad on several waypoints.
            serde_json::json!({"waypoint_ids": ["waypoint-1", "waypoint-9"]}),
        );
        log_waypoint_effect(
            &store,
            "scheduler",
            "delivered queued waypoint injection(s)",
            Some("squad-3"),
            Some("cell-0"),
            serde_json::json!({"waypoint_ids": ["waypoint-1"]}),
        );
        // Another waypoint's effect must not leak in.
        log_waypoint_effect(
            &store,
            "scheduler",
            "cell halted by waypoint block",
            Some("squad-4"),
            Some("cell-0"),
            serde_json::json!({"waypoint_id": "waypoint-other"}),
        );

        let events = store.waypoint_deliveries("waypoint-1").unwrap();
        let sources: BTreeSet<&str> = events.iter().map(|e| e.source.as_str()).collect();
        assert!(
            sources.contains("scheduler") && sources.contains("submit"),
            "the feed must carry scheduler and submit effects, got {sources:?}"
        );
        assert_eq!(
            events.len(),
            4,
            "four effects name this waypoint: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.squad_id.as_deref() == Some("squad-4")),
            "another waypoint's effects must not leak in"
        );
        // The entity refs are what make this a per-squad/per-cell view rather
        // than a flat log.
        assert!(
            events
                .iter()
                .any(|e| e.cell_id.as_deref() == Some("cell-0") && e.message.contains("halted")),
            "a halt must be attributable to the cell it stopped: {events:?}"
        );
    }

    /// Work that was rebased onto the change already has it. Telling it to go
    /// make the change would send it to redo work its own tree already
    /// reflects, so the guidance has to arrive framed as notice, not as a
    /// task.
    #[test]
    fn guidance_delivered_after_a_rebase_says_so() {
        let injections = vec![PendingInjectionView {
            id: 1,
            target_squad: "squad-1".to_string(),
            target_task: 0,
            target_idx: 0,
            payload: "rename greet() to salute()".to_string(),
            status: "queued".to_string(),
            batch_id: None,
            waypoint_id: Some("waypoint-1".to_string()),
            created_at_ms: 0,
            updated_at_ms: 0,
        }];

        let plain = render_injection_block(&injections, false);
        assert!(plain.contains("rename greet() to salute()"));
        assert!(
            !plain.contains("rebased onto the branch"),
            "an injection that did not follow a rebase must not claim one: {plain}"
        );

        let rebased = render_injection_block(&injections, true);
        assert!(rebased.contains("rename greet() to salute()"));
        assert!(
            rebased.contains("rebased onto the branch"),
            "a post-rebase injection must say the change is probably already present: {rebased}"
        );
        assert!(
            rebased.contains("nothing here for you to do"),
            "it must say doing nothing is a legitimate outcome: {rebased}"
        );
    }

    /// A resumed cell is re-invoked with no memory of having been held, so
    /// the guidance has to say what happened and what is wanted. The rebased
    /// wording matters most: the change is already in its tree, and an agent
    /// told only "make this change" would set about remaking it.
    #[test]
    fn resume_guidance_states_the_change_landed_and_asks_for_an_answer() {
        let waypoint = WaypointView {
            id: "waypoint-1".to_string(),
            label: Some("rename greet".to_string()),
            prompt: "greet() is renamed to salute().".to_string(),
            agent: None,
            model: None,
            allow_advisory: false,
            state: "open".to_string(),
            created_at_ms: 0,
            updated_at_ms: 0,
            closed_at_ms: None,
        };

        let rebased = render_resume_guidance(&waypoint, true);
        assert!(rebased.contains("rename greet"), "{rebased}");
        assert!(
            rebased.contains("greet() is renamed to salute()."),
            "{rebased}"
        );
        assert!(
            rebased.contains("rebased onto the branch carrying that change"),
            "a rebased resume must say the change is already present: {rebased}"
        );
        assert!(
            rebased.contains("nothing here for you to implement"),
            "it must name doing nothing as a legitimate outcome: {rebased}"
        );
        assert!(
            rebased.contains("RALPHUS_BEARING:"),
            "a held cell must be told how to release itself: {rebased}"
        );

        // Without a rebase the change may genuinely be absent, so the
        // opposite instruction applies -- claiming it was already delivered
        // would be a lie the agent cannot check cheaply.
        let plain = render_resume_guidance(&waypoint, false);
        assert!(
            !plain.contains("rebased onto the branch carrying that change"),
            "{plain}"
        );
        assert!(plain.contains("was not rebased"), "{plain}");
        assert!(plain.contains("RALPHUS_BEARING:"), "{plain}");
    }

    /// A review's approval is held until it answers, so there has to BE a way
    /// for a review to answer. Until the resolver's reply was read for a
    /// bearing, there was not: the hold could only be lifted by closing the
    /// waypoint or demoting the entry to advisory, and the error message told
    /// the operator to do something impossible.
    #[test]
    fn a_review_can_answer_and_release_its_own_approval() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Block,
            )
            .unwrap();

        assert_eq!(
            store.review_block_gating_waypoint("guardian-1").unwrap(),
            Some("waypoint-1".to_string()),
            "held until it answers"
        );

        store
            .set_affected_bearing_decision(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                BearingDecision::Accepted,
            )
            .unwrap();

        assert_eq!(
            store.review_block_gating_waypoint("guardian-1").unwrap(),
            None,
            "answering releases the approval it was holding"
        );
    }

    /// The review lookup must be as scoped as the squad one -- an answer from
    /// one review must not satisfy a waypoint that never affected it.
    #[test]
    fn open_waypoints_affecting_review_is_scoped_to_that_review() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        open_waypoint(&store, "waypoint-2");
        insert_bare_guardian(&store, "guardian-1");
        insert_bare_guardian(&store, "guardian-2");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-2",
                WaypointEntryKind::Review,
                "guardian-2",
                AffectedMode::Block,
            )
            .unwrap();

        assert_eq!(
            store.open_waypoints_affecting_review("guardian-1").unwrap(),
            vec!["waypoint-1".to_string()]
        );
        // A squad id must never match a review entry, and vice versa.
        assert!(
            store
                .open_waypoints_affecting_squad("guardian-1")
                .unwrap()
                .is_empty()
        );
    }

    /// An injection is only delivered when its target cell is next
    /// dispatched. A cell that has not run yet would otherwise receive
    /// guidance long after the waypoint it speaks for closed -- telling an
    /// agent to coordinate around something already finished.
    #[test]
    fn closing_stops_delivering_guidance_that_was_still_queued() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Done);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        store
            .enqueue_injection(
                "squad-1",
                0,
                0,
                "coordinate with the rename",
                Some("bearing-1"),
                Some("waypoint-1"),
            )
            .unwrap();

        assert!(store.close_waypoint_manually("waypoint-1").unwrap());

        assert!(
            store.drain_injections("squad-1", 0, 0).unwrap().is_empty(),
            "a closed waypoint must not still have guidance waiting to deliver"
        );
    }

    // ---- being released is news too ---------------------------------------

    /// Someone told their work is held has to hear when it stops being held.
    /// Closing lifts every hold at once, and for a long time said nothing --
    /// so the only message anyone got was the one announcing the hold, which
    /// reads from the outside like a dead end.
    #[test]
    fn closing_a_waypoint_tells_the_work_it_was_holding() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-held", SquadState::Done);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-held",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .set_affected_bearing_decision(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-held",
                BearingDecision::Accepted,
            )
            .unwrap();

        let before = mailbox_len(&store);
        assert!(store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert!(
            mailbox_len(&store) > before,
            "closing must notify what it was holding"
        );
    }

    /// An advisory entry was never held, so telling it that it has been
    /// released would be describing something that never happened.
    #[test]
    fn closing_says_nothing_to_work_it_was_never_holding() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-advised", SquadState::Done);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-advised",
                AffectedMode::Advisory,
            )
            .unwrap();

        let before = mailbox_len(&store);
        assert!(store.maybe_auto_close_waypoint("waypoint-1").unwrap());
        assert_eq!(
            mailbox_len(&store),
            before,
            "an advisory entry was never held, so it was not released"
        );
    }

    // ---- phase 1 releases, phase 2 closes ---------------------------------

    /// Affected work is held because the change it must take up does not
    /// exist yet. Once the affected lands it has to be released -- it cannot
    /// answer the waypoint while the waypoint is preventing it from running,
    /// and phase 2 is waiting on exactly that answer.
    #[test]
    fn a_block_hold_lifts_once_the_affected_lands() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-goal", SquadState::Pending);
        insert_bare_squad(&store, "squad-downstream", SquadState::Pending);
        store
            .add_roster_entry("waypoint-1", WaypointEntryKind::Squad, "squad-goal", None)
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-downstream",
                AffectedMode::Block,
            )
            .unwrap();

        assert_eq!(
            store
                .squad_block_gating_waypoint("squad-downstream")
                .unwrap(),
            Some("waypoint-1".to_string()),
            "held while the waypoint's own work is unfinished"
        );

        store
            .set_squad_state("squad-goal", SquadState::Done)
            .unwrap();

        assert_eq!(
            store
                .squad_block_gating_waypoint("squad-downstream")
                .unwrap(),
            None,
            "released the moment the affected lands, so it can do the work and answer"
        );
    }

    /// A waypoint that names no affected is a broadcast: there is no phase-1
    /// change to wait for, so its affected work is never held -- only asked.
    #[test]
    fn an_empty_affected_never_holds_affected_work() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-downstream", SquadState::Pending);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-downstream",
                AffectedMode::Block,
            )
            .unwrap();
        assert_eq!(
            store
                .squad_block_gating_waypoint("squad-downstream")
                .unwrap(),
            None
        );
    }

    /// The two phases are separate gates, and both must pass. A landed affected
    /// alone does not close a waypoint whose affected work never answered --
    /// that was the whole failure mode of keeping one list.
    #[test]
    fn a_landed_affected_does_not_close_a_waypoint_nobody_answered() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-goal", SquadState::Done);
        insert_bare_squad(&store, "squad-downstream", SquadState::Done);
        store
            .add_roster_entry("waypoint-1", WaypointEntryKind::Squad, "squad-goal", None)
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-downstream",
                AffectedMode::Block,
            )
            .unwrap();

        assert!(store.roster_complete("waypoint-1").unwrap());
        assert!(!store.affected_have_answered("waypoint-1").unwrap());
        assert!(!store.maybe_auto_close_waypoint("waypoint-1").unwrap());

        answer(
            &store,
            "waypoint-1",
            WaypointEntryKind::Squad,
            "squad-downstream",
        );
        assert!(store.maybe_auto_close_waypoint("waypoint-1").unwrap());
    }

    /// Declining is an answer. A waypoint must be able to finish even when
    /// the work it landed on decided not to take it up -- otherwise
    /// "rejected" would just be a way to hang the waypoint forever.
    #[test]
    fn a_rejected_decision_satisfies_phase_two_like_any_other() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-downstream", SquadState::Done);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-downstream",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .set_affected_bearing_decision(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-downstream",
                BearingDecision::Rejected,
            )
            .unwrap();
        assert!(store.affected_have_answered("waypoint-1").unwrap());
        assert!(store.maybe_auto_close_waypoint("waypoint-1").unwrap());
    }

    /// An advisory entry is told what changed and left alone. Waiting on one
    /// to answer would make advisory mode mean nothing.
    #[test]
    fn an_advisory_entry_owes_no_answer() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-advised", SquadState::Done);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-advised",
                AffectedMode::Advisory,
            )
            .unwrap();
        assert!(store.affected_have_answered("waypoint-1").unwrap());
        assert!(store.maybe_auto_close_waypoint("waypoint-1").unwrap());
    }

    // ---- re-survey after a settings edit ---------------------------------

    /// A re-survey only re-judges what the *daemon* enrolled. A human
    /// declaration is never second-guessed by the classifier, so clearing an
    /// explicit entry's verdict would strand it: never re-judged, and now
    /// reading as unsurveyed to a gate that fails closed.
    #[test]
    fn clearing_verdicts_for_a_resurvey_spares_human_declared_entries() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-explicit",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-auto",
                AffectedMode::Block,
            )
            .unwrap();
        for entry in ["squad-explicit", "squad-auto"] {
            store
                .set_affected_survey_result(
                    "waypoint-1",
                    WaypointEntryKind::Squad,
                    entry,
                    &SurveyVerdict {
                        impacted: false,
                        mode: AffectedMode::Block,
                        rationale: "unrelated".to_string(),
                    },
                )
                .unwrap();
        }

        assert_eq!(
            store
                .clear_auto_enrolled_survey_verdicts("waypoint-1")
                .unwrap(),
            1,
            "only the daemon-enrolled entry should be re-queued"
        );

        let affected = store.list_affected_entries("waypoint-1").unwrap();
        let verdict = |id: &str| {
            affected
                .iter()
                .find(|e| e.entry_id == id)
                .expect("entry present")
                .survey_verdict
                .clone()
        };
        assert_eq!(verdict("squad-auto"), None, "re-queued for the classifier");
        assert_eq!(
            verdict("squad-explicit"),
            Some("not_impacted".to_string()),
            "a human declaration keeps its verdict"
        );
    }

    /// A cleared entry becomes a survey candidate again -- that is the whole
    /// mechanism by which an edited waypoint gets re-judged, so it is worth
    /// pinning rather than assuming.
    #[test]
    fn a_cleared_entry_is_a_survey_candidate_again() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        // Scope the waypoint repo-wide in project "core" via a seed entry, so
        // `squad-auto` is inside it and can be a candidate at all.
        insert_bare_squad(&store, "squad-seed", SquadState::Pending);
        insert_bare_task(&store, "squad-seed", 0, "core");
        insert_bare_cell(&store, "squad-seed", 0, 0, None, None);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();
        insert_bare_squad(&store, "squad-auto", SquadState::Pending);
        insert_bare_task(&store, "squad-auto", 0, "core");
        insert_bare_cell(&store, "squad-auto", 0, 0, None, None);
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-auto",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-auto",
                &SurveyVerdict {
                    impacted: true,
                    mode: AffectedMode::Block,
                    rationale: "touches it".to_string(),
                },
            )
            .unwrap();
        let judged = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert!(
            !judged.iter().any(|c| c.entry_id == "squad-auto"),
            "a judged entry is settled, not a candidate"
        );

        store
            .clear_auto_enrolled_survey_verdicts("waypoint-1")
            .unwrap();

        let requeued = store.waypoint_survey_candidates("waypoint-1").unwrap();
        assert!(
            requeued.iter().any(|c| c.entry_id == "squad-auto"),
            "clearing the verdict must put it back in the classifier's queue"
        );
    }

    /// The preview must say the same thing the clear then does -- it is the
    /// only warning anyone gets before accepting a re-survey.
    #[test]
    fn the_resurvey_preview_matches_what_the_clear_actually_touches() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-explicit",
                AffectedMode::Block,
            )
            .unwrap();
        for entry in ["squad-auto-a", "squad-auto-b"] {
            store
                .enroll_affected_entry(
                    "waypoint-1",
                    WaypointEntryKind::Squad,
                    entry,
                    AffectedMode::Block,
                )
                .unwrap();
        }

        let preview = store.resurvey_preview("waypoint-1").unwrap();
        let mut previewed: Vec<&str> = preview
            .targets
            .iter()
            .map(|t| t.entry_id.as_str())
            .collect();
        previewed.sort_unstable();
        assert_eq!(previewed, ["squad-auto-a", "squad-auto-b"]);
        assert_eq!(
            preview
                .held_explicit
                .iter()
                .map(|t| t.entry_id.as_str())
                .collect::<Vec<_>>(),
            ["squad-explicit"],
            "the explicit entry must still be reported, as left alone"
        );
        assert_eq!(
            preview.targets.len(),
            store
                .clear_auto_enrolled_survey_verdicts("waypoint-1")
                .unwrap(),
            "the preview promised a count the clear must honour"
        );
    }

    /// A block-mode entry with no verdict is held by the gate, so a re-survey
    /// can re-hold work the old guidance had released. The preview has to say
    /// so, or accepting it is not an informed choice.
    #[test]
    fn the_preview_flags_entries_a_resurvey_would_re_hold() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        // A squad's hold lasts while the waypoint's own work is unfinished, so
        // give it an unfinished goal to be held against.
        insert_bare_squad(&store, "squad-goal", SquadState::Pending);
        store
            .add_roster_entry("waypoint-1", WaypointEntryKind::Squad, "squad-goal", None)
            .unwrap();
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-blocked",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-advisory",
                AffectedMode::Advisory,
            )
            .unwrap();
        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-blocked",
                &SurveyVerdict {
                    impacted: false,
                    mode: AffectedMode::Block,
                    rationale: "cleared".to_string(),
                },
            )
            .unwrap();

        let preview = store.resurvey_preview("waypoint-1").unwrap();
        let flag = |id: &str| {
            preview
                .targets
                .iter()
                .find(|t| t.entry_id == id)
                .expect("previewed")
                .will_be_held_until_judged
        };
        assert!(flag("squad-blocked"), "a block-mode entry is re-held");
        assert!(!flag("squad-advisory"), "an advisory entry is never held");

        // And the gate agrees: once cleared, the released entry blocks again.
        assert_eq!(
            store.squad_block_gating_waypoint("squad-blocked").unwrap(),
            None,
            "a not_impacted verdict clears the gate"
        );
        store
            .clear_auto_enrolled_survey_verdicts("waypoint-1")
            .unwrap();
        assert_eq!(
            store.squad_block_gating_waypoint("squad-blocked").unwrap(),
            Some("waypoint-1".to_string()),
            "clearing the verdict re-holds it, exactly as the preview warned"
        );
    }

    /// An unknown waypoint is a 404, not an empty preview -- which would read
    /// as "this would do nothing" and invite a save that then fails.
    #[test]
    fn the_resurvey_preview_rejects_an_unknown_waypoint() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.resurvey_preview("waypoint-nope"),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn the_grouped_delivery_count_agrees_with_the_affected_it_summarises() {
        // The list endpoint counts delivery statuses in SQL instead of
        // hydrating each affected, which means the status strings are written in
        // two places. They must not drift -- `via-restack` is hyphenated in the
        // column and underscored in the JSON field, which is exactly the kind
        // of pair that silently stops matching.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        for (entry, status) in [
            ("squad-a", DeliveryStatus::Delivered),
            ("squad-b", DeliveryStatus::ViaRestack),
            ("squad-c", DeliveryStatus::Failed),
            ("squad-d", DeliveryStatus::Undelivered),
        ] {
            store
                .add_affected_entry(
                    "waypoint-1",
                    WaypointEntryKind::Squad,
                    entry,
                    AffectedMode::Block,
                )
                .unwrap();
            store
                .set_affected_delivery_status("waypoint-1", WaypointEntryKind::Squad, entry, status)
                .unwrap();
        }

        let counts = store.waypoint_delivery_counts().unwrap();
        let mine = counts.get("waypoint-1").expect("counted");
        for status in [
            DeliveryStatus::Delivered,
            DeliveryStatus::ViaRestack,
            DeliveryStatus::Failed,
            DeliveryStatus::Undelivered,
        ] {
            assert_eq!(
                mine.get(status.as_str()).copied().unwrap_or(0),
                1,
                "{} must be counted under the string the column stores",
                status.as_str()
            );
        }
    }

    #[test]
    fn an_effect_spanning_several_waypoints_appears_under_each_of_them() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        open_waypoint(&store, "waypoint-2");
        log_waypoint_effect(
            &store,
            "submit",
            "squad enrolled pending survey",
            Some("squad-1"),
            None,
            serde_json::json!({"waypoint_ids": ["waypoint-1", "waypoint-2"]}),
        );
        for id in ["waypoint-1", "waypoint-2"] {
            assert_eq!(
                store.waypoint_deliveries(id).unwrap().len(),
                1,
                "{id} must see the shared effect"
            );
        }
    }

    // ── survey transport: terminal agents via the runner ─────────────────

    /// A `Runner` that answers whatever the survey asks with a canned reply,
    /// and records the spec it was handed so a test can assert how the survey
    /// invoked it.
    struct SurveyTestRunner {
        reply: String,
        seen: std::sync::Mutex<Vec<crate::runner::RunnerSpec>>,
    }

    impl SurveyTestRunner {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_string(),
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    impl crate::runner::Runner for SurveyTestRunner {
        fn run(&self, spec: &crate::runner::RunnerSpec) -> crate::runner::RunnerResult {
            self.seen.lock().unwrap().push(spec.clone());
            crate::runner::RunnerResult {
                status: "done".to_string(),
                summary: self.reply.clone(),
                error: None,
                ..crate::runner::RunnerResult::failure("unused")
            }
        }
    }

    /// A squad with a real `cwd`, which is what selects the runner transport
    /// (no cwd means no process can be spawned, so the direct API is used).
    fn insert_squad_with_cwd(store: &Store, squad_id: &str, cwd: &str) {
        insert_bare_squad(store, squad_id, SquadState::Running);
        insert_bare_task(store, squad_id, 0, "core");
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, prompt, cwd)
                 VALUES(?,0,0,'s0-0','claude-code','running','rewrite the greet callers',?)",
                params![squad_id, cwd],
            )
            .unwrap();
    }

    #[test]
    fn direct_chat_owns_only_the_api_backends() {
        // The two transports must never disagree about which backend is
        // theirs, or a terminal agent would be handed to the chat API (hard
        // error, fail-closed block) or vice versa.
        for api in ["claude", "anthropic", "ollama", "CLAUDE", "Ollama"] {
            assert!(direct_chat_handles(api), "{api} is an API backend");
        }
        for terminal in ["claude-code", "claude-cli", "codex", "codex-cli", "pi"] {
            assert!(
                !direct_chat_handles(terminal),
                "{terminal} is a terminal executable and must go through the runner"
            );
        }
    }

    #[test]
    fn a_terminal_agent_survey_is_classified_through_the_runner() {
        // Before this, `claude-code` hit the chat API's unsupported-agent
        // error on every call. The survey is fail-closed, so that resolved to
        // impacted + block -- meaning such a waypoint silently blocked every
        // piece of work it covered. It must now classify for real.
        let store = Store::open_in_memory().unwrap();
        store
            .create_waypoint(
                "waypoint-1",
                Some("rename"),
                "greet() is being renamed",
                Some("claude-code"),
                Some("sonnet"),
                false,
            )
            .unwrap();
        insert_squad_with_cwd(&store, "squad-1", "/repo");
        let handle = handle(store);
        let runner =
            SurveyTestRunner::new("IMPACTED: no\nRATIONALE: this work only touches documentation");
        let as_runner: Arc<dyn Runner> = runner.clone();

        let verdict = survey_candidate(
            &handle,
            "waypoint-1",
            &SurveyCandidate {
                kind: WaypointEntryKind::Squad,
                entry_id: "squad-1".to_string(),
            },
            &Cancellations::new(),
            &as_runner,
        )
        .unwrap();

        assert!(
            !verdict.impacted,
            "the runner's real verdict must be used, not a fail-closed default"
        );
        assert!(verdict.rationale.contains("documentation"));

        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one runner invocation");
        let spec = &seen[0];
        assert_eq!(spec.agent, "claude-code");
        assert_eq!(spec.cwd, "/repo", "must run in the candidate's own cwd");
        assert!(
            spec.prompt
                .as_deref()
                .is_some_and(|p| p.contains("rewrite the greet callers")),
            "the candidate description must reach the agent: {:?}",
            spec.prompt
        );
        assert!(
            spec.system_prompt
                .as_deref()
                .is_some_and(|s| s.contains("IMPACTED")),
            "the reply-format contract belongs in the system prompt: {:?}",
            spec.system_prompt
        );
        assert_eq!(
            spec.timeout_sec,
            Some(SURVEY_RUNNER_TIMEOUT_SECS),
            "a terminal agent has no bound of its own; the survey must impose one"
        );
    }

    #[test]
    fn a_terminal_agent_survey_that_returns_nothing_fails_closed() {
        // The fail-closed contract still governs the runner transport: an
        // agent that produces no classifiable reply must block, never silently
        // release the gate.
        let store = Store::open_in_memory().unwrap();
        store
            .create_waypoint(
                "waypoint-1",
                None,
                "greet() is being renamed",
                Some("codex"),
                None,
                false,
            )
            .unwrap();
        insert_squad_with_cwd(&store, "squad-1", "/repo");
        let handle = handle(store);
        let as_runner: Arc<dyn Runner> = SurveyTestRunner::new("   ");

        let verdict = survey_candidate(
            &handle,
            "waypoint-1",
            &SurveyCandidate {
                kind: WaypointEntryKind::Squad,
                entry_id: "squad-1".to_string(),
            },
            &Cancellations::new(),
            &as_runner,
        )
        .unwrap();

        assert!(verdict.impacted, "an unclassifiable reply must fail closed");
        assert_eq!(verdict.mode, AffectedMode::Block);
    }

    #[test]
    fn a_sweep_defers_candidates_past_its_per_sweep_cap_instead_of_fanning_out() {
        // A RepoWide waypoint's candidate set is every non-terminal squad in
        // the project, and each survey can now be a real paid agent process,
        // so the sweep must bound its fan-out. Deferred candidates keep their
        // NULL verdict, which still blocks -- the cap costs latency, not a
        // missed gate.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cwd(&store, "squad-seed", "/repo");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-seed",
                AffectedMode::Block,
            )
            .unwrap();
        let over = SURVEY_MAX_PER_SWEEP + 4;
        for n in 0..over {
            insert_squad_with_cwd(&store, &format!("squad-c{n}"), "/repo");
        }
        assert!(
            store
                .waypoint_survey_candidates("waypoint-1")
                .unwrap()
                .len()
                > SURVEY_MAX_PER_SWEEP,
            "precondition: more candidates than one sweep may dispatch"
        );

        let handle = handle(store);
        let runner = SurveyTestRunner::new("IMPACTED: no\nRATIONALE: unrelated");
        let as_runner: Arc<dyn Runner> = runner.clone();
        run_pending_surveys(&handle, &Cancellations::new(), &as_runner);

        // Threads are spawned, so wait for the dispatched batch to land rather
        // than racing it; the assertion is on the cap, not on timing.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while runner.seen.lock().unwrap().len() < SURVEY_MAX_PER_SWEEP
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        let dispatched = runner.seen.lock().unwrap().len();
        assert!(
            dispatched <= SURVEY_MAX_PER_SWEEP,
            "one sweep must never dispatch more than {SURVEY_MAX_PER_SWEEP}, got {dispatched}"
        );
    }

    #[test]
    fn survey_cwd_comes_from_the_candidates_own_work() {
        let store = Store::open_in_memory().unwrap();
        insert_squad_with_cwd(&store, "squad-1", "/repo/checkout");
        assert_eq!(
            survey_candidate_cwd(&store, WaypointEntryKind::Squad, "squad-1").as_deref(),
            Some("/repo/checkout")
        );
        insert_bare_guardian(&store, "guardian-1");
        assert!(
            survey_candidate_cwd(&store, WaypointEntryKind::Review, "guardian-1").is_some(),
            "a review resolves to its guardian's git root"
        );
        assert_eq!(
            survey_candidate_cwd(&store, WaypointEntryKind::Squad, "squad-missing"),
            None,
            "an unknown candidate has no cwd, which falls back to the direct API"
        );
    }

    // ── stale flagging + redo (scenario 4) ───────────────────────────────

    /// Seed a squad with one task and one cell, in the given cell state, so
    /// redo/injection tests have real cells to act on.
    fn insert_squad_with_cell(
        store: &Store,
        squad_id: &str,
        squad_state: SquadState,
        cell_state: &str,
    ) {
        insert_bare_squad(store, squad_id, squad_state);
        insert_bare_task(store, squad_id, 0, "core");
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, prompt, cwd)
                 VALUES(?,0,0,'s0-0','claude-code',?,'do the work','')",
                params![squad_id, cell_state],
            )
            .unwrap();
    }

    fn seed_bearing(store: &Store, waypoint_id: &str, summary: &str) -> BearingView {
        store
            .append_waypoint_bearing(
                waypoint_id,
                WaypointEntryKind::Squad,
                "squad-producer",
                summary,
                None,
                Some("abc1234"),
                Some("rename greet to salute"),
            )
            .unwrap()
    }

    #[test]
    fn stale_sweep_flags_impacted_work_that_finished_while_its_waypoint_was_open() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-1", SquadState::Done, "done");
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                &SurveyVerdict {
                    impacted: true,
                    mode: AffectedMode::Block,
                    rationale: "touches the renamed function".to_string(),
                },
            )
            .unwrap();
        // The waypoint stays OPEN: this squad finished without its guidance.

        let handle = handle(store);
        run_pending_stale_notices(&handle);

        let store = handle.lock();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert!(
            entries[0].stale_at_ms.is_some(),
            "finished impacted work must be flagged stale once the waypoint closes"
        );
        let broadcast = store
            .mailbox_messages_for_client("client", false, None)
            .unwrap();
        assert_eq!(broadcast.len(), 1, "exactly one stale notice");
        assert!(
            broadcast[0].message.contains("ralphus waypoint redo"),
            "the stale notice must state the next step: {}",
            broadcast[0].message
        );
    }

    #[test]
    fn stale_sweep_skips_not_impacted_and_still_running_entries() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        // Finished, but the survey cleared it -> nothing to redo.
        insert_squad_with_cell(&store, "squad-clear", SquadState::Done, "done");
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-clear",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-clear",
                &SurveyVerdict {
                    impacted: false,
                    mode: AffectedMode::Block,
                    rationale: "unrelated".to_string(),
                },
            )
            .unwrap();
        // Impacted, but still running -> not finished, so not stale.
        insert_squad_with_cell(&store, "squad-live", SquadState::Running, "running");
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-live",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-live",
                &SurveyVerdict {
                    impacted: true,
                    mode: AffectedMode::Block,
                    rationale: "touches it".to_string(),
                },
            )
            .unwrap();

        let handle = handle(store);
        run_pending_stale_notices(&handle);

        let store = handle.lock();
        for entry in store.list_affected_entries("waypoint-1").unwrap() {
            assert!(
                entry.stale_at_ms.is_none(),
                "{} must not be flagged stale",
                entry.entry_id
            );
        }
    }

    #[test]
    fn stale_sweep_does_not_flag_work_that_only_ran_after_its_waypoint_closed() {
        // The gate's healthy case, and the reason this sweep keys on the
        // waypoint still being *open*: a squad the gate correctly held until
        // the waypoint closed then ran against the landed changes, so it is
        // not stale. Sweeping closed waypoints instead would flag every
        // correctly-gated squad in the project.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-gated", SquadState::Done, "done");
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-gated",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-gated",
                &SurveyVerdict {
                    impacted: true,
                    mode: AffectedMode::Block,
                    rationale: "touches it".to_string(),
                },
            )
            .unwrap();
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);
        run_pending_stale_notices(&handle);

        let store = handle.lock();
        assert!(
            store.list_affected_entries("waypoint-1").unwrap()[0]
                .stale_at_ms
                .is_none(),
            "work that ran after the waypoint closed already has its changes"
        );
        assert!(
            store
                .mailbox_messages_for_client("client", false, None)
                .unwrap()
                .is_empty(),
            "no stale notice for correctly-gated work"
        );
    }

    #[test]
    fn a_redo_clears_the_stale_flag_so_a_later_sweep_does_not_re_flag_it() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-1", SquadState::Done, "done");
        store
            .enroll_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        store
            .set_affected_survey_result(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                &SurveyVerdict {
                    impacted: true,
                    mode: AffectedMode::Block,
                    rationale: "touches it".to_string(),
                },
            )
            .unwrap();

        let handle = handle(store);
        run_pending_stale_notices(&handle);
        {
            let store = handle.lock();
            redo_affected_entry(&store, "waypoint-1", "squad-1").unwrap();
            assert!(
                store.list_affected_entries("waypoint-1").unwrap()[0]
                    .stale_at_ms
                    .is_none(),
                "a redo must clear the stale flag"
            );
        }
        // The squad is back to pending, so it is no longer terminal and the
        // sweep has nothing to re-flag. Re-running must stay quiet.
        run_pending_stale_notices(&handle);
        let store = handle.lock();
        assert!(
            store.list_affected_entries("waypoint-1").unwrap()[0]
                .stale_at_ms
                .is_none(),
            "a re-queued squad must not be re-flagged as stale"
        );
        let broadcast = store
            .mailbox_messages_for_client("client", false, None)
            .unwrap();
        assert_eq!(broadcast.len(), 1, "the stale notice must not be re-sent");
    }

    #[test]
    fn a_redo_requeues_the_squad_and_folds_bearings_in_without_losing_prior_findings() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-1", SquadState::Done, "done");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        seed_bearing(&store, "waypoint-1", "salute() now lives in src/greet.py");

        // The previous run's own findings, which the redo must carry forward
        // rather than replace -- that is the whole point of redoing rather
        // than resubmitting from scratch.
        let uri = crate::ghost::cell_uri("squad-1", 0, 0);
        store
            .upsert_ghost(
                &uri,
                crate::ghost::KIND_CELL,
                Some("squad-1"),
                None,
                "prior finding: the caller list lives in src/app.py",
                None,
            )
            .unwrap();

        redo_affected_entry(&store, "waypoint-1", "squad-1").unwrap();

        assert_eq!(
            store.squad_state("squad-1").unwrap(),
            SquadState::Pending,
            "a redo must reset the squad to pending"
        );
        let ghost = store.get_ghost(&uri).unwrap().expect("ghost present");
        assert!(
            ghost.content.contains("prior finding"),
            "the previous run's findings must survive the redo: {}",
            ghost.content
        );
        assert!(
            ghost.content.contains("salute() now lives in src/greet.py"),
            "the waypoint's bearings must be folded in: {}",
            ghost.content
        );
    }

    #[test]
    fn a_redo_carries_the_previous_runs_own_prophecies_into_the_new_iteration() {
        // The point of redoing rather than resubmitting is to keep what the
        // last run learned. Prophecies are where an agent records exactly
        // that, so a redo that dropped them would throw away the run's most
        // valuable output and the new agent would start cold.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-1", SquadState::Done, "done");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        seed_bearing(&store, "waypoint-1", "salute() now lives in src/greet.py");

        let uri = crate::ghost::cell_uri("squad-1", 0, 0);
        store
            .add_prophecy(
                &uri,
                0,
                crate::prophecy::ProphecyKind::Discovery,
                "the caller list lives only in src/app.py",
                None,
                Some("squad-1"),
                None,
            )
            .unwrap();
        store
            .add_prophecy(
                &uri,
                0,
                crate::prophecy::ProphecyKind::Hazard,
                "the import in src/app.py is not re-exported anywhere",
                None,
                Some("squad-1"),
                None,
            )
            .unwrap();

        redo_affected_entry(&store, "waypoint-1", "squad-1").unwrap();

        let ghost = store.get_ghost(&uri).unwrap().expect("ghost present");
        assert!(
            ghost
                .content
                .contains("the caller list lives only in src/app.py"),
            "the previous run's discovery must be carried forward: {}",
            ghost.content
        );
        assert!(
            ghost
                .content
                .contains("the import in src/app.py is not re-exported anywhere"),
            "every prophecy must be carried, not just the first: {}",
            ghost.content
        );
        assert!(
            ghost.content.contains("[hazard]"),
            "a prophecy's kind must survive, so the agent can weigh it: {}",
            ghost.content
        );
        assert!(
            ghost.content.contains("salute() now lives in src/greet.py"),
            "the waypoint's bearings must still be folded in alongside: {}",
            ghost.content
        );
    }

    #[test]
    fn a_redo_with_no_prophecies_still_folds_bearings_and_adds_no_empty_section() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-1", SquadState::Done, "done");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        seed_bearing(&store, "waypoint-1", "salute() now lives in src/greet.py");

        redo_affected_entry(&store, "waypoint-1", "squad-1").unwrap();

        let ghost = store
            .get_ghost(&crate::ghost::cell_uri("squad-1", 0, 0))
            .unwrap()
            .expect("ghost present");
        assert!(ghost.content.contains("salute() now lives in src/greet.py"));
        assert!(
            !ghost.content.contains("previous run"),
            "no empty findings section when the cell recorded no prophecies: {}",
            ghost.content
        );
    }

    #[test]
    fn a_redo_of_a_review_entry_is_rejected_with_a_pointer_to_its_squad() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Block,
            )
            .unwrap();
        let err = redo_affected_entry(&store, "waypoint-1", "guardian-1").unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidTransition(ref m) if m.contains("redo the squad")),
            "a review-kind redo must be refused and point at its squad, got {err:?}"
        );
    }

    #[test]
    fn a_redo_of_an_unknown_entry_is_not_found() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        assert!(matches!(
            redo_affected_entry(&store, "waypoint-1", "squad-nope").unwrap_err(),
            StoreError::NotFound
        ));
    }

    // ── advisory bearing injection (advisory in-flight delivery) ─────────

    #[test]
    fn an_advisory_bearing_queues_one_injection_per_unfinished_cell() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-1", SquadState::Running, "running");
        // A second, still-pending cell in the same squad: the realistic way an
        // advisory note reaches work that hasn't started its turn yet.
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, prompt, cwd)
                 VALUES('squad-1',0,1,'s0-1','claude-code','pending','later work','')",
                [],
            )
            .unwrap();
        // ...and a finished one, which must be skipped.
        store
            .conn
            .execute(
                "INSERT INTO cells(squad_id, task_idx, idx, sid, agent, state, prompt, cwd)
                 VALUES('squad-1',0,2,'s0-2','claude-code','done','old work','')",
                [],
            )
            .unwrap();
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();

        let bearing = seed_bearing(&store, "waypoint-1", "salute() replaces greet()");
        let queued = store
            .queue_advisory_bearing_injections("waypoint-1", &bearing)
            .unwrap();
        assert_eq!(queued, 2, "only the two unfinished cells get an injection");

        let drained = store.drain_injections("squad-1", 0, 0).unwrap();
        assert_eq!(drained.len(), 1);
        assert!(
            drained[0].payload.contains("salute() replaces greet()"),
            "payload must carry the bearing text: {}",
            drained[0].payload
        );
        assert!(
            drained[0].payload.contains("abc1234"),
            "payload must keep the bearing's commit reference as an investigation lead: {}",
            drained[0].payload
        );
        assert!(
            store.drain_injections("squad-1", 0, 0).unwrap().is_empty(),
            "delivery must be exactly-once"
        );
        assert!(
            store.drain_injections("squad-1", 0, 2).unwrap().is_empty(),
            "a finished cell must never have been queued"
        );
    }

    #[test]
    fn a_block_mode_entry_gets_no_advisory_injection() {
        // Blocking entries receive bearings through the halt path's ghost-fold
        // instead; queueing for them too would double-deliver.
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-1", SquadState::Running, "running");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        let bearing = seed_bearing(&store, "waypoint-1", "guidance");
        assert_eq!(
            store
                .queue_advisory_bearing_injections("waypoint-1", &bearing)
                .unwrap(),
            0
        );
        assert!(store.drain_injections("squad-1", 0, 0).unwrap().is_empty());
    }

    #[test]
    fn a_superseded_bearing_batch_can_be_withdrawn_before_delivery() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-1", SquadState::Running, "running");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        let bearing = seed_bearing(&store, "waypoint-1", "guidance");
        store
            .queue_advisory_bearing_injections("waypoint-1", &bearing)
            .unwrap();
        assert_eq!(
            store
                .cancel_injection_batch(&format!("bearing-{}", bearing.id))
                .unwrap(),
            1,
            "one bearing's injections share a batch id, so they cancel together"
        );
        assert!(store.drain_injections("squad-1", 0, 0).unwrap().is_empty());
    }

    #[test]
    fn the_rendered_injection_block_frames_guidance_as_advisory() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_squad_with_cell(&store, "squad-1", SquadState::Running, "running");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        let bearing = seed_bearing(&store, "waypoint-1", "salute() replaces greet()");
        store
            .queue_advisory_bearing_injections("waypoint-1", &bearing)
            .unwrap();
        let drained = store.drain_injections("squad-1", 0, 0).unwrap();
        let block = render_injection_block(&drained, false);
        assert!(block.contains("does not block this cell"));
        assert!(block.contains("salute() replaces greet()"));
        assert!(
            block.contains("Inspect the current state of the code"),
            "must keep WAYPOINT_SYSTEM_PROMPT's don't-assume-it's-local contract: {block}"
        );
    }

    // ── run_pending_stand_down_notices (Phase 6) ─────────────────────────

    #[test]
    fn stand_down_sweep_notifies_an_advisory_squad_entry_once_closed() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Done);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);

        run_pending_stand_down_notices(&handle);

        let store = handle.lock();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert!(
            entries[0].stand_down_at_ms.is_some(),
            "an advisory squad entry must be marked stood-down once notified"
        );
        let broadcast = store
            .mailbox_messages_for_client("client", false, None)
            .unwrap();
        assert_eq!(broadcast.len(), 1);
        // The notice is waypoint-advisory-channel traffic, not a generic squad
        // attribute change: someone filtering their mail for waypoint guidance
        // should see the stand-down alongside the advice it retires.
        assert_eq!(broadcast[0].event_kind.as_deref(), Some("waypoint_advised"));
    }

    #[test]
    fn stand_down_sweep_sends_feedback_to_an_advisory_review_entry_with_a_ready_branch() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        open_review_branch(&store, "guardian-1", "feature-x");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);

        run_pending_stand_down_notices(&handle);

        let store = handle.lock();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert!(
            entries[0].stand_down_at_ms.is_some(),
            "a review entry with a ready branch must be marked stood-down once feedback is sent"
        );
    }

    #[test]
    fn stand_down_sweep_leaves_a_review_entry_pending_without_a_ready_branch() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_guardian(&store, "guardian-1");
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Review,
                "guardian-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);

        run_pending_stand_down_notices(&handle);

        let store = handle.lock();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        // Previously this was deferred until the review had a built worktree,
        // because the notice went out over the feedback path and that path
        // needs somewhere to write. A mailbox row has no such precondition, so
        // a review with no ready branch is notified immediately rather than
        // waiting for a worktree it may never build.
        assert!(
            entries[0].stand_down_at_ms.is_some(),
            "a watcher notice needs no ready branch, so it sends on the first sweep"
        );
        assert_eq!(
            mail(&store)[0].entity_uri.as_deref(),
            Some("guardian:guardian-1")
        );
    }

    #[test]
    fn stand_down_sweep_never_notifies_a_block_mode_entry() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Done);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Block,
            )
            .unwrap();
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);

        run_pending_stand_down_notices(&handle);

        let store = handle.lock();
        let entries = store.list_affected_entries("waypoint-1").unwrap();
        assert!(
            entries[0].stand_down_at_ms.is_none(),
            "block-mode entries never get a stand-down notice -- gating simply lifting is the signal"
        );
        let broadcast = store
            .mailbox_messages_for_client("client", false, None)
            .unwrap();
        assert!(broadcast.is_empty());
    }

    #[test]
    fn stand_down_sweep_is_idempotent_across_repeated_runs() {
        let store = Store::open_in_memory().unwrap();
        open_waypoint(&store, "waypoint-1");
        insert_bare_squad(&store, "squad-1", SquadState::Done);
        store
            .add_affected_entry(
                "waypoint-1",
                WaypointEntryKind::Squad,
                "squad-1",
                AffectedMode::Advisory,
            )
            .unwrap();
        assert!(store.close_waypoint("waypoint-1").unwrap());

        let handle = handle(store);

        run_pending_stand_down_notices(&handle);
        run_pending_stand_down_notices(&handle);

        let store = handle.lock();
        let broadcast = store
            .mailbox_messages_for_client("client", false, None)
            .unwrap();
        assert_eq!(
            broadcast.len(),
            1,
            "an already-stood-down entry must be skipped on a later sweep"
        );
    }
}
