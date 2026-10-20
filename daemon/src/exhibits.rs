//! Exhibits: the pictures, recordings and output a reviewer should see in a
//! PR/MR description, captured by the review's live actions.
//!
//! * An **exhibit request** asks for one thing to be shown. The agent that
//!   made a change writes them inside its live handoff as
//!   `EXHIBIT: <kind> | <what to show>` lines; [`split_handoff`] lifts them
//!   out of the handoff text, and they follow their handoff through
//!   deduplication into exactly one live action.
//! * Each stored request is an **exhibit slot** of that action. A PR/MR shows
//!   the slots of the actions attributed to its branch inside a managed
//!   `## Exhibits` section, one marker-delimited block per slot
//!   ([`render_section`], [`splice_exhibits`]). Only the owning action's run
//!   ever fills a slot, and only the text between that slot's markers is
//!   rewritten, so two actions can never overwrite each other's work.
//! * A slot's **fill** is what the run delivered: an uploaded file's URL, a
//!   text excerpt, or the reason nothing was captured. A re-run replaces it.

use crate::store::Result;

/// The line prefix of an exhibit request inside a live handoff.
pub const REQUEST_MARKER: &str = "EXHIBIT:";

/// Opens the managed exhibits section of a PR/MR description.
pub const SECTION_START: &str = "<!-- ralphus:exhibits:start -->";
/// Closes the managed exhibits section of a PR/MR description.
pub const SECTION_END: &str = "<!-- ralphus:exhibits:end -->";

/// The most slots one live action keeps; later requests are dropped.
pub const MAX_PER_ACTION: usize = 4;

/// Longest `describe` text kept for one request.
const DESCRIBE_MAX_CHARS: usize = 300;

/// Longest text excerpt an `output` slot shows.
const OUTPUT_MAX_CHARS: usize = 4_000;

/// What a slot asks to show. `Gif`, `Video` and `Screenshot` are visual and
/// are delivered in the review's preference order; `Output` is text, shown
/// inline and never uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExhibitKind {
    Gif,
    Video,
    Screenshot,
    Output,
}

impl ExhibitKind {
    /// Every kind, in declaration order.
    pub const ALL: [ExhibitKind; 4] = [Self::Gif, Self::Video, Self::Screenshot, Self::Output];

    /// The stored/config lowercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Video => "video",
            Self::Screenshot => "screenshot",
            Self::Output => "output",
        }
    }

    /// Parse a kind name, ignoring case and surrounding space.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::ALL
            .into_iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(name))
    }

    /// Whether this kind is a picture or recording (everything but `output`).
    #[must_use]
    pub fn is_visual(self) -> bool {
        self != Self::Output
    }

    /// How the kind is named in a PR/MR description.
    #[must_use]
    pub fn display(self) -> &'static str {
        match self {
            Self::Gif => "GIF",
            Self::Video => "Video",
            Self::Screenshot => "Screenshot",
            Self::Output => "Output",
        }
    }
}

/// Where a request came from. Earlier origins win when a PR's slot cap cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExhibitOrigin {
    /// Written by the agent that made the change, in its live handoff.
    Handoff,
    /// A configured `[[exhibit.recipe]]` matching the changed files.
    Recipe,
    /// Added by the AI exhibit check when the PR was written.
    Inferred,
}

impl ExhibitOrigin {
    /// The stored lowercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Handoff => "handoff",
            Self::Recipe => "recipe",
            Self::Inferred => "inferred",
        }
    }

    fn parse(name: &str) -> Self {
        match name {
            "recipe" => Self::Recipe,
            "inferred" => Self::Inferred,
            _ => Self::Handoff,
        }
    }
}

/// One thing a reviewer should be shown.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExhibitRequest {
    pub kind: ExhibitKind,
    pub describe: String,
}

impl ExhibitRequest {
    /// The identity two requests are duplicates under: same kind, same
    /// description ignoring case and runs of whitespace.
    fn dedupe_key(&self) -> (ExhibitKind, String) {
        (
            self.kind,
            self.describe
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase(),
        )
    }
}

/// A handoff with its exhibit requests lifted out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitHandoff {
    /// The handoff text without its `EXHIBIT:` lines.
    pub text: String,
    /// The usable requests, in order, at most [`MAX_PER_ACTION`].
    pub requests: Vec<ExhibitRequest>,
    /// `EXHIBIT:` lines that could not be used (unknown kind, nothing to
    /// show, over the cap), verbatim, for the log.
    pub rejected: Vec<String>,
}

/// Separate a live handoff's `EXHIBIT: <kind> | <what to show>` lines from
/// its test steps.
#[must_use]
pub fn split_handoff(text: &str) -> SplitHandoff {
    let mut out = SplitHandoff::default();
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix(REQUEST_MARKER) else {
            kept.push(line);
            continue;
        };
        let parsed = rest.split_once('|').and_then(|(kind, describe)| {
            let describe = describe.trim();
            let kind = ExhibitKind::parse(kind)?;
            (!describe.is_empty()).then(|| ExhibitRequest {
                kind,
                describe: describe.chars().take(DESCRIBE_MAX_CHARS).collect(),
            })
        });
        match parsed {
            Some(request) if out.requests.len() < MAX_PER_ACTION => {
                if !out
                    .requests
                    .iter()
                    .any(|r| r.dedupe_key() == request.dedupe_key())
                {
                    out.requests.push(request);
                }
            }
            _ => out.rejected.push(line.trim().to_string()),
        }
    }
    out.text = kept.join("\n").trim().to_string();
    out
}

/// The requests of several handoffs merged into one live action: duplicates
/// dropped, order kept, at most [`MAX_PER_ACTION`].
#[must_use]
pub fn merge_requests<'a>(
    groups: impl IntoIterator<Item = &'a [ExhibitRequest]>,
) -> Vec<ExhibitRequest> {
    let mut out: Vec<ExhibitRequest> = Vec::new();
    for request in groups.into_iter().flatten() {
        if out.len() == MAX_PER_ACTION {
            break;
        }
        if !out.iter().any(|r| r.dedupe_key() == request.dedupe_key()) {
            out.push(request.clone());
        }
    }
    out
}

// ---- storage ----------------------------------------------------------------

/// A slot's position: the live action that owns it and its index there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct SlotKey {
    pub action_idx: i64,
    pub slot_idx: i64,
}

/// What a run delivered for one slot.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ExhibitFill {
    /// The kind actually delivered, which can be lower in the preference
    /// order than the one requested.
    pub delivered_kind: Option<ExhibitKind>,
    /// Where an uploaded file lives.
    pub url: Option<String>,
    /// What the reviewer should look at.
    pub caption: Option<String>,
    /// An `output` slot's text.
    pub text: Option<String>,
    /// Why nothing was delivered.
    pub missing_reason: Option<String>,
    /// The captured file, kept in the daemon's state directory until it is
    /// uploaded.
    pub local_path: Option<String>,
    /// The forge's id for the upload, when the forge can delete uploads
    /// (GitLab); used to remove the file a re-run replaces.
    pub forge_upload_id: Option<String>,
    /// Upload ids of fills this one replaced, deleted from the forge once
    /// this fill's own upload is in place (never before).
    pub stale_upload_ids: Vec<String>,
    /// The live run that delivered this fill.
    pub run_id: Option<String>,
    pub updated_at_ms: i64,
}

/// One slot of a review: its request, origin and current fill.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExhibitSlot {
    pub key: SlotKey,
    pub request: ExhibitRequest,
    pub origin: ExhibitOrigin,
    /// Why an inferred request was added.
    pub why: Option<String>,
    pub fill: Option<ExhibitFill>,
}

/// Every exhibit slot of a review, ordered by action and slot.
///
/// # Errors
///
/// On a database failure.
pub fn guardian_exhibit_slots_conn(
    conn: &rusqlite::Connection,
    guardian_id: &str,
) -> Result<Vec<ExhibitSlot>> {
    let mut stmt = conn.prepare(
        "SELECT r.action_idx, r.slot_idx, r.kind, r.describe, r.origin, r.why,
                f.delivered_kind, f.url, f.caption, f.text, f.missing_reason,
                f.forge_upload_id, f.run_id, f.updated_at_ms, f.local_path,
                f.stale_upload_ids
         FROM guardian_exhibit_requests r
         LEFT JOIN guardian_exhibit_fills f
           ON f.guardian_id = r.guardian_id
          AND f.action_idx = r.action_idx
          AND f.slot_idx = r.slot_idx
         WHERE r.guardian_id = ?
         ORDER BY r.action_idx, r.slot_idx",
    )?;
    let rows = stmt.query_map(rusqlite::params![guardian_id], |r| {
        let kind: String = r.get(2)?;
        let origin: String = r.get(4)?;
        let updated: Option<i64> = r.get(13)?;
        let delivered: Option<String> = r.get(6)?;
        let stale: Option<String> = r.get(15)?;
        Ok((
            SlotKey {
                action_idx: r.get(0)?,
                slot_idx: r.get(1)?,
            },
            kind,
            r.get::<_, String>(3)?,
            origin,
            r.get::<_, Option<String>>(5)?,
            updated.map(|updated_at_ms| ExhibitFill {
                delivered_kind: delivered.as_deref().and_then(ExhibitKind::parse),
                url: r.get(7).ok().flatten(),
                caption: r.get(8).ok().flatten(),
                text: r.get(9).ok().flatten(),
                missing_reason: r.get(10).ok().flatten(),
                local_path: r.get(14).ok().flatten(),
                forge_upload_id: r.get(11).ok().flatten(),
                stale_upload_ids: stale
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default(),
                run_id: r.get(12).ok().flatten(),
                updated_at_ms,
            }),
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (key, kind, describe, origin, why, fill) = row?;
        // A kind this build no longer knows is skipped rather than guessed.
        let Some(kind) = ExhibitKind::parse(&kind) else {
            continue;
        };
        out.push(ExhibitSlot {
            key,
            request: ExhibitRequest { kind, describe },
            origin: ExhibitOrigin::parse(&origin),
            why,
            fill,
        });
    }
    Ok(out)
}

impl crate::store::Store {
    /// Every exhibit slot of a review.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn guardian_exhibit_slots(&self, guardian_id: &str) -> Result<Vec<ExhibitSlot>> {
        guardian_exhibit_slots_conn(&self.conn, guardian_id)
    }

    /// Append requests to a live action's slots. Requests that duplicate an
    /// existing slot of the action are skipped, and the action never holds
    /// more than [`MAX_PER_ACTION`]. Returns the keys of the slots created.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn add_exhibit_requests(
        &self,
        guardian_id: &str,
        action_idx: i64,
        origin: ExhibitOrigin,
        requests: &[(ExhibitRequest, Option<String>)],
    ) -> Result<Vec<SlotKey>> {
        let existing: Vec<ExhibitSlot> = guardian_exhibit_slots_conn(&self.conn, guardian_id)?
            .into_iter()
            .filter(|s| s.key.action_idx == action_idx)
            .collect();
        let mut keys: Vec<(ExhibitKind, String)> =
            existing.iter().map(|s| s.request.dedupe_key()).collect();
        let mut next = existing.iter().map(|s| s.key.slot_idx + 1).max().unwrap_or(0);
        let now = crate::store::now_ms();
        let mut created = Vec::new();
        for (request, why) in requests {
            if keys.len() >= MAX_PER_ACTION || keys.contains(&request.dedupe_key()) {
                continue;
            }
            self.conn.execute(
                "INSERT INTO guardian_exhibit_requests
                    (guardian_id, action_idx, slot_idx, kind, describe, origin, why, created_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    guardian_id,
                    action_idx,
                    next,
                    request.kind.as_str(),
                    request.describe,
                    origin.as_str(),
                    why,
                    now
                ],
            )?;
            keys.push(request.dedupe_key());
            created.push(SlotKey {
                action_idx,
                slot_idx: next,
            });
            next += 1;
        }
        Ok(created)
    }

    /// Record what a run delivered for a slot, replacing any earlier fill.
    /// The replaced fill's upload id (and any it was still waiting to delete)
    /// carries over into the new fill's `stale_upload_ids`, so the old file is
    /// removed from the forge only after the new one is uploaded. Returns the
    /// replaced fill.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn set_exhibit_fill(
        &self,
        guardian_id: &str,
        key: SlotKey,
        fill: &ExhibitFill,
    ) -> Result<Option<ExhibitFill>> {
        let previous = guardian_exhibit_slots_conn(&self.conn, guardian_id)?
            .into_iter()
            .find(|s| s.key == key)
            .and_then(|s| s.fill);
        let mut stale = fill.stale_upload_ids.clone();
        if let Some(prev) = &previous {
            for id in prev.stale_upload_ids.iter().chain(prev.forge_upload_id.iter()) {
                if Some(id) != fill.forge_upload_id.as_ref() && !stale.contains(id) {
                    stale.push(id.clone());
                }
            }
        }
        self.write_exhibit_fill(guardian_id, key, fill, &stale)?;
        Ok(previous)
    }

    fn write_exhibit_fill(
        &self,
        guardian_id: &str,
        key: SlotKey,
        fill: &ExhibitFill,
        stale: &[String],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO guardian_exhibit_fills
                (guardian_id, action_idx, slot_idx, delivered_kind, url, caption, text,
                 missing_reason, local_path, forge_upload_id, stale_upload_ids, run_id,
                 updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(guardian_id, action_idx, slot_idx) DO UPDATE SET
                delivered_kind=excluded.delivered_kind, url=excluded.url,
                caption=excluded.caption, text=excluded.text,
                missing_reason=excluded.missing_reason, local_path=excluded.local_path,
                forge_upload_id=excluded.forge_upload_id,
                stale_upload_ids=excluded.stale_upload_ids, run_id=excluded.run_id,
                updated_at_ms=excluded.updated_at_ms",
            rusqlite::params![
                guardian_id,
                key.action_idx,
                key.slot_idx,
                fill.delivered_kind.map(ExhibitKind::as_str),
                fill.url,
                fill.caption,
                fill.text,
                fill.missing_reason,
                fill.local_path,
                fill.forge_upload_id,
                (!stale.is_empty())
                    .then(|| serde_json::to_string(stale).unwrap_or_else(|_| "[]".into())),
                fill.run_id,
                fill.updated_at_ms
            ],
        )?;
        Ok(())
    }

    /// Record that a slot's captured file is now on the forge.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn record_exhibit_upload(
        &self,
        guardian_id: &str,
        key: SlotKey,
        url: &str,
        upload_id: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE guardian_exhibit_fills SET url=?, forge_upload_id=?
             WHERE guardian_id=? AND action_idx=? AND slot_idx=?",
            rusqlite::params![url, upload_id, guardian_id, key.action_idx, key.slot_idx],
        )?;
        Ok(())
    }

    /// Forget stale upload ids once they are deleted from the forge (or
    /// cannot be, as on GitHub).
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn clear_stale_exhibit_uploads(
        &self,
        guardian_id: &str,
        key: SlotKey,
        removed: &[String],
    ) -> Result<()> {
        let Some(slot) = guardian_exhibit_slots_conn(&self.conn, guardian_id)?
            .into_iter()
            .find(|s| s.key == key)
        else {
            return Ok(());
        };
        let Some(fill) = slot.fill else {
            return Ok(());
        };
        let keep: Vec<String> = fill
            .stale_upload_ids
            .iter()
            .filter(|id| !removed.contains(id))
            .cloned()
            .collect();
        self.write_exhibit_fill(guardian_id, key, &fill, &keep)
    }

    /// Whether a review has any exhibit slot.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn guardian_has_exhibits(&self, guardian_id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM guardian_exhibit_requests WHERE guardian_id=?",
            rusqlite::params![guardian_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

// ---- settings ---------------------------------------------------------------

/// The default cap on slots one PR/MR shows.
pub const DEFAULT_MAX_PER_PR: usize = 6;

/// The default length cap of a delivered video, in seconds.
pub const DEFAULT_MAX_VIDEO_SECONDS: u32 = 15;

/// The default order visual kinds are delivered in.
pub const DEFAULT_KIND_PREFERENCE: [ExhibitKind; 3] =
    [ExhibitKind::Gif, ExhibitKind::Video, ExhibitKind::Screenshot];

/// A review's resolved exhibit settings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExhibitSettings {
    /// Whether PR/MR descriptions show exhibits at all.
    pub enabled: bool,
    /// The most slots one PR/MR shows (always at least 1).
    pub max_per_pr: usize,
    /// Whether the PR writer may add exhibit requests of its own.
    pub infer: bool,
    /// Visual kinds, most preferred first.
    pub kind_preference: Vec<ExhibitKind>,
    /// The longest a delivered video may run.
    pub max_video_seconds: u32,
}

impl Default for ExhibitSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_per_pr: DEFAULT_MAX_PER_PR,
            infer: true,
            kind_preference: DEFAULT_KIND_PREFERENCE.to_vec(),
            max_video_seconds: DEFAULT_MAX_VIDEO_SECONDS,
        }
    }
}

/// The exhibit slots `pr` shows, each with its owning action's label, or none
/// when exhibits are turned off for the review.
///
/// # Errors
///
/// On a database failure.
pub fn pr_slots(
    store: &crate::store_lock::StoreHandle,
    pr: &crate::pr::PullRequestView,
    guardian: &crate::guardian::GuardianView,
) -> Result<Vec<(ExhibitSlot, String)>> {
    let settings = crate::exhibit_settings::resolve(store, guardian);
    if !settings.enabled {
        return Ok(Vec::new());
    }
    let guard = store.lock();
    let slots = guard.guardian_exhibit_slots(&guardian.id)?;
    if slots.is_empty() {
        return Ok(Vec::new());
    }
    let labels: std::collections::HashMap<i64, String> = guard
        .guardian_live_actions(&guardian.id)?
        .into_iter()
        .map(|a| (a.idx, a.label))
        .collect();
    let attribution = |action: i64| {
        guard
            .live_action_branch_ids(&guardian.id, action)
            .unwrap_or_default()
    };
    Ok(slots_for_pr(&slots, pr.branch_id.as_deref(), &attribution, settings.max_per_pr)
        .into_iter()
        .map(|slot| {
            let label = labels.get(&slot.key.action_idx).cloned().unwrap_or_default();
            (slot, label)
        })
        .collect())
}

// ---- which slots a PR shows -----------------------------------------------

/// The slots one PR/MR shows: those whose action is attributed to `branch_id`
/// (`None` = the combined PR, which shows every slot; an action with no
/// attribution shows on every PR, the same rule live-test results use), capped
/// at `max_per_pr` with handoff requests kept before recipe and inferred ones.
/// The result is ordered by action and slot.
#[must_use]
pub fn slots_for_pr(
    slots: &[ExhibitSlot],
    branch_id: Option<&str>,
    attribution: &dyn Fn(i64) -> Vec<String>,
    max_per_pr: usize,
) -> Vec<ExhibitSlot> {
    let mut on_pr: Vec<&ExhibitSlot> = slots
        .iter()
        .filter(|slot| match branch_id {
            None => true,
            Some(branch) => {
                let attributed = attribution(slot.key.action_idx);
                attributed.is_empty() || attributed.iter().any(|b| b == branch)
            }
        })
        .collect();
    on_pr.sort_by_key(|s| (s.origin, s.key));
    on_pr.truncate(max_per_pr);
    let mut out: Vec<ExhibitSlot> = on_pr.into_iter().cloned().collect();
    out.sort_by_key(|s| s.key);
    out
}

// ---- the AI exhibit check at PR time ---------------------------------------

/// The reply marker the PR writer adds an exhibit request under.
pub const INFER_MARKER: &str = "EXHIBIT_REQUEST:";

/// The most requests the PR writer may add for one PR.
const MAX_INFERRED_PER_PR: usize = 4;

/// Longest cell prompt excerpt the PR writer is shown.
const PROMPT_EXCERPT_CHARS: usize = 1_500;

/// Who an inferred request is for: one of the listed live actions, or a new
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferredOwner {
    Action(i64),
    New,
}

/// One exhibit request the PR writer added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredRequest {
    pub owner: InferredOwner,
    pub request: ExhibitRequest,
    pub why: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawInferred {
    owner: serde_json::Value,
    kind: String,
    describe: String,
    #[serde(default)]
    why: Option<String>,
}

/// The prompt section that asks the PR writer which exhibits a reviewer of
/// `branch_id` needs beyond those already requested, or `None` when the
/// review turns exhibits or the check off.
#[must_use]
pub fn inference_prompt(
    store: &crate::store_lock::StoreHandle,
    guardian: &crate::guardian::GuardianView,
    branch_id: &str,
) -> Option<String> {
    let settings = crate::exhibit_settings::resolve(store, guardian);
    if !settings.enabled || !settings.infer {
        return None;
    }
    let guard = store.lock();
    let prompts: Vec<String> = guard
        .conn
        .prepare(
            "SELECT c.prompt FROM cells c
             JOIN guardian_branches gb ON gb.guardian_id = ?1 AND gb.branch = c.review_branch
             WHERE gb.id = ?2 AND c.prompt IS NOT NULL AND c.prompt != ''
             ORDER BY c.squad_id, c.task_idx, c.idx LIMIT 3",
        )
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![guardian.id, branch_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .unwrap_or_default();
    let actions: Vec<crate::live_actions::LiveAction> = guard
        .guardian_live_actions(&guardian.id)
        .unwrap_or_default()
        .into_iter()
        .filter(|a| {
            let attributed = guard
                .live_action_branch_ids(&guardian.id, a.idx)
                .unwrap_or_default();
            attributed.is_empty() || attributed.iter().any(|b| b == branch_id)
        })
        .collect();
    let slots = guard.guardian_exhibit_slots(&guardian.id).unwrap_or_default();
    drop(guard);
    let mut out = String::from(
        "\n\nREVIEWER EXHIBITS. A separate agent will run this change live and capture \
         pictures, recordings or output for the reviewer. Decide whether a reviewer needs \
         to SEE anything beyond what is already requested below -- the final visible \
         result of a UI, render or command this diff changes. Ask for nothing when the \
         change has nothing visible, and never repeat a request that is already listed.",
    );
    if !prompts.is_empty() {
        out.push_str("\n\nWhat the work on this branch was asked to do:\n");
        for p in &prompts {
            let excerpt: String = p.chars().take(PROMPT_EXCERPT_CHARS).collect();
            out.push_str(&format!("---\n{}\n", excerpt.trim()));
        }
    }
    if actions.is_empty() {
        out.push_str("\nNo live tests exist yet for this branch; use owner \"new\".\n");
    } else {
        out.push_str("\nLive tests for this branch (owner id: label):\n");
        for a in &actions {
            out.push_str(&format!("- {}: {}\n", a.idx, one_line(&a.label)));
            for s in slots.iter().filter(|s| s.key.action_idx == a.idx) {
                out.push_str(&format!(
                    "    already requested: {} | {}\n",
                    s.request.kind.as_str(),
                    one_line(&s.request.describe)
                ));
            }
        }
    }
    out.push_str(&format!(
        "\nFor each additional exhibit (at most {MAX_INFERRED_PER_PR}), add one line BEFORE \
         the ===DESCRIPTION=== line: {INFER_MARKER} {{\"owner\": <live test id, or \"new\">, \
         \"kind\": \"gif\" | \"video\" | \"screenshot\" | \"output\", \"describe\": \"<what \
         to show>\", \"why\": \"<why the reviewer needs it>\"}}. Prefer the live test that \
         already exercises that part of the program."
    ));
    Some(out)
}

/// The PR writer's exhibit requests: every `EXHIBIT_REQUEST:` line before the
/// description marker, at most [`MAX_INFERRED_PER_PR`].
#[must_use]
pub fn parse_inferred(reply: &str) -> Vec<InferredRequest> {
    let head = reply.split("===DESCRIPTION===").next().unwrap_or_default();
    let mut out: Vec<InferredRequest> = Vec::new();
    for line in head.lines() {
        let Some(rest) = line.trim_start().strip_prefix(INFER_MARKER) else {
            continue;
        };
        let Ok(raw) = serde_json::from_str::<RawInferred>(rest.trim()) else {
            continue;
        };
        let Some(kind) = ExhibitKind::parse(&raw.kind) else {
            continue;
        };
        let describe = raw.describe.trim();
        if describe.is_empty() {
            continue;
        }
        let owner = match &raw.owner {
            serde_json::Value::Number(n) => n.as_i64().map_or(InferredOwner::New, InferredOwner::Action),
            serde_json::Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map_or(InferredOwner::New, InferredOwner::Action),
            _ => InferredOwner::New,
        };
        let request = ExhibitRequest {
            kind,
            describe: describe.chars().take(DESCRIBE_MAX_CHARS).collect(),
        };
        if out.iter().any(|r| r.request.dedupe_key() == request.dedupe_key()) {
            continue;
        }
        out.push(InferredRequest {
            owner,
            request,
            why: raw.why.map(|w| one_line(&w)).filter(|w| !w.is_empty()),
        });
        if out.len() == MAX_INFERRED_PER_PR {
            break;
        }
    }
    out
}

impl crate::store::Store {
    /// Attach the PR writer's requests for `branch_id`: to the named live
    /// action when it exists and covers the branch, else to the branch's first
    /// live action. With no live action for the branch yet, a request waits
    /// as pending until the review's live actions are written
    /// ([`Self::assign_pending_exhibits`]). Returns how many became slots now.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn apply_inferred_exhibits(
        &self,
        guardian_id: &str,
        branch_id: &str,
        requests: &[InferredRequest],
    ) -> Result<usize> {
        let actions = self.guardian_live_actions(guardian_id)?;
        let covers = |idx: i64| {
            let attributed = self.live_action_branch_ids(guardian_id, idx).unwrap_or_default();
            attributed.is_empty() || attributed.iter().any(|b| b == branch_id)
        };
        let branch_actions: Vec<i64> = actions.iter().map(|a| a.idx).filter(|i| covers(*i)).collect();
        let mut attached = 0;
        for r in requests {
            let owner = match r.owner {
                InferredOwner::Action(idx) if branch_actions.contains(&idx) => Some(idx),
                _ => branch_actions.first().copied(),
            };
            match owner {
                Some(idx) => {
                    attached += self
                        .add_exhibit_requests(
                            guardian_id,
                            idx,
                            ExhibitOrigin::Inferred,
                            &[(r.request.clone(), r.why.clone())],
                        )?
                        .len();
                }
                None => {
                    self.conn.execute(
                        "INSERT INTO guardian_pending_exhibits
                            (guardian_id, branch_id, kind, describe, why, created_at_ms)
                         VALUES (?, ?, ?, ?, ?, ?)",
                        rusqlite::params![
                            guardian_id,
                            branch_id,
                            r.request.kind.as_str(),
                            r.request.describe,
                            r.why,
                            crate::store::now_ms()
                        ],
                    )?;
                }
            }
        }
        Ok(attached)
    }

    /// Turn pending inferred requests into slots once a review's live actions
    /// exist: each joins the first live action covering its branch, and a
    /// branch with no live action gets one more action of its own, which
    /// exists only to capture those exhibits. Called right after the review's
    /// live actions are first written.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn assign_pending_exhibits(&self, guardian_id: &str) -> Result<()> {
        let pending: Vec<(i64, String, String, String, Option<String>)> = self
            .conn
            .prepare(
                "SELECT rowid, branch_id, kind, describe, why FROM guardian_pending_exhibits
                 WHERE guardian_id=? ORDER BY rowid",
            )?
            .query_map(rusqlite::params![guardian_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if pending.is_empty() {
            return Ok(());
        }
        let mut by_branch: std::collections::BTreeMap<String, Vec<(ExhibitRequest, Option<String>)>> =
            std::collections::BTreeMap::new();
        for (_, branch, kind, describe, why) in &pending {
            if let Some(kind) = ExhibitKind::parse(kind) {
                by_branch.entry(branch.clone()).or_default().push((
                    ExhibitRequest {
                        kind,
                        describe: describe.clone(),
                    },
                    why.clone(),
                ));
            }
        }
        for (branch_id, requests) in by_branch {
            let actions = self.guardian_live_actions(guardian_id)?;
            let owner = actions.iter().map(|a| a.idx).find(|idx| {
                self.live_action_branch_ids(guardian_id, *idx)
                    .unwrap_or_default()
                    .iter()
                    .any(|b| b == &branch_id)
            });
            let idx = match owner {
                Some(idx) => idx,
                None => {
                    let idx = actions.iter().map(|a| a.idx + 1).max().unwrap_or(0);
                    let branch_name: String = self
                        .conn
                        .query_row(
                            "SELECT branch FROM guardian_branches WHERE guardian_id=? AND id=?",
                            rusqlite::params![guardian_id, branch_id],
                            |r| r.get(0),
                        )
                        .unwrap_or_else(|_| branch_id.clone());
                    let prompt = format!(
                        "Run the program this branch ({branch_name}) changes the way a user would, \
                         and show its result for the reviewer. Start it the way the project's \
                         AGENTS.md or README says."
                    );
                    self.conn.execute(
                        "INSERT INTO guardian_live_actions
                            (guardian_id, idx, label, prompt, source_cells, created_at_ms)
                         VALUES (?, ?, ?, ?, '[]', ?)",
                        rusqlite::params![
                            guardian_id,
                            idx,
                            format!("Exhibits: {branch_name}"),
                            prompt,
                            crate::store::now_ms()
                        ],
                    )?;
                    self.conn.execute(
                        "INSERT OR IGNORE INTO guardian_live_action_branches
                            (guardian_id, action_idx, branch_id) VALUES (?, ?, ?)",
                        rusqlite::params![guardian_id, idx, branch_id],
                    )?;
                    idx
                }
            };
            self.add_exhibit_requests(guardian_id, idx, ExhibitOrigin::Inferred, &requests)?;
        }
        self.conn.execute(
            "DELETE FROM guardian_pending_exhibits WHERE guardian_id=?",
            rusqlite::params![guardian_id],
        )?;
        Ok(())
    }
}

// ---- a live run's side: asking for and reading back exhibits ---------------

/// The reply marker a live-action agent reports one slot under.
pub const REPORT_MARKER: &str = "RALPHUS_EXHIBIT:";

/// The environment variable naming the directory a run saves exhibits into.
pub const DIR_ENV: &str = "RALPHUS_EXHIBIT_DIR";

/// How a slot is named to an agent: `<action>.<slot>`.
#[must_use]
pub fn slot_label(key: SlotKey) -> String {
    format!("{}.{}", key.action_idx, key.slot_idx)
}

/// The prompt section that asks a live run to capture its action's slots.
#[must_use]
pub fn run_prompt_section(
    slots: &[ExhibitSlot],
    settings: &ExhibitSettings,
    dir: &std::path::Path,
) -> String {
    let order = settings
        .kind_preference
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(", then ");
    let mut out = format!(
        "## Exhibits for the reviewer\n\
After testing, capture what a code reviewer must see, one file per item below, \
saved in {} (also in ${DIR_ENV}). Capture the final visible result, not the setup. \
For a visual item deliver the first kind of {order} that shows it; keep any video \
under {} seconds and any GIF short (a few seconds, small window). For an output \
item save the relevant text as a .txt file. If the steps you ran did not show an \
item, run whatever else shows it now. Use the project's own capture tools when it has \
them; otherwise `ralphus exhibit screenshot --pid <process id> -o <name>.png` or \
`ralphus exhibit record --pid <process id> --seconds 5 -o <name>.gif` (or --screen \
instead of --pid) captures a window you started into that directory.\n",
        dir.display(),
        settings.max_video_seconds,
    );
    for slot in slots {
        out.push_str(&format!(
            "- {} ({}): {}\n",
            slot_label(slot.key),
            slot.request.kind.as_str(),
            one_line(&slot.request.describe)
        ));
    }
    out.push_str(&format!(
        "\nFor every item, end your reply with one line `{REPORT_MARKER} ` followed by a \
single-line JSON object: {{\"slot\": \"<id>\", \"file\": \"<file name in that \
directory>\", \"caption\": \"<what to look at, one sentence>\"}}, or \
{{\"slot\": \"<id>\", \"missing\": \"<why it could not be captured>\"}}. Put these \
lines before the RALPHUS_LIVE_RESULT line."
    ));
    out
}

/// The prompt for the one follow-up turn that asks for slots a run left
/// unreported.
#[must_use]
pub fn followup_prompt(
    missing: &[ExhibitSlot],
    settings: &ExhibitSettings,
    dir: &std::path::Path,
    earlier_report: Option<&str>,
) -> String {
    let mut out = String::from(
        "Your live test is done. These reviewer exhibits are still missing; capture them \
now, running whatever your test skipped. Do not change any code.\n\n",
    );
    if let Some(report) = earlier_report.filter(|r| !r.trim().is_empty()) {
        out.push_str("What the live test found:\n");
        out.push_str(report.trim());
        out.push_str("\n\n");
    }
    out.push_str(&run_prompt_section(missing, settings, dir));
    out
}

/// One slot as a run reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotReport {
    /// A file saved in the run's exhibit directory.
    File {
        key: SlotKey,
        file: String,
        caption: Option<String>,
    },
    /// Nothing could be captured.
    Missing { key: SlotKey, reason: String },
}

impl SlotReport {
    #[must_use]
    pub fn key(&self) -> SlotKey {
        match self {
            Self::File { key, .. } | Self::Missing { key, .. } => *key,
        }
    }
}

#[derive(serde::Deserialize)]
struct RawReport {
    slot: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    missing: Option<String>,
}

/// The reports of a reply that name a slot of `action_idx` in `slots`, the
/// last one per slot winning. Reports naming any other slot -- another
/// action's, or one that does not exist -- are returned separately and never
/// used: a run only ever fills its own action's slots.
#[must_use]
pub fn parse_reports(
    reply: &str,
    action_idx: i64,
    slots: &[ExhibitSlot],
) -> (Vec<SlotReport>, Vec<String>) {
    let mut kept: Vec<SlotReport> = Vec::new();
    let mut ignored = Vec::new();
    for line in reply.lines() {
        let Some(rest) = line.trim_start().strip_prefix(REPORT_MARKER) else {
            continue;
        };
        let parsed = serde_json::from_str::<RawReport>(rest.trim()).ok();
        let key = parsed.as_ref().and_then(|raw| {
            let (a, s) = raw.slot.trim().split_once('.')?;
            let key = SlotKey {
                action_idx: a.trim().parse().ok()?,
                slot_idx: s.trim().parse().ok()?,
            };
            (key.action_idx == action_idx && slots.iter().any(|slot| slot.key == key))
                .then_some(key)
        });
        let report = match (parsed, key) {
            (Some(raw), Some(key)) => match (raw.file, raw.missing) {
                (Some(file), _) if !file.trim().is_empty() => Some(SlotReport::File {
                    key,
                    file: file.trim().to_string(),
                    caption: raw.caption.filter(|c| !c.trim().is_empty()),
                }),
                (_, Some(reason)) => Some(SlotReport::Missing {
                    key,
                    reason: one_line(&reason),
                }),
                _ => None,
            },
            _ => None,
        };
        match report {
            Some(report) => {
                kept.retain(|r| r.key() != report.key());
                kept.push(report);
            }
            None => ignored.push(line.trim().to_string()),
        }
    }
    (kept, ignored)
}

/// The file a report names, when it really is a regular file directly inside
/// `dir` (no `..`, no absolute path, no symlink escaping it).
#[must_use]
pub fn resolve_reported_file(dir: &std::path::Path, file: &str) -> Option<std::path::PathBuf> {
    let name = std::path::Path::new(file).file_name()?;
    if std::path::Path::new(file).components().count() != 1 {
        return None;
    }
    let path = dir.join(name);
    let canonical_dir = dir.canonicalize().ok()?;
    let canonical = path.canonicalize().ok()?;
    (canonical.parent() == Some(canonical_dir.as_path()) && canonical.is_file()).then_some(canonical)
}

// ---- rendering --------------------------------------------------------------

/// The marker that opens one slot's block.
#[must_use]
pub fn slot_start(key: SlotKey) -> String {
    format!(
        "<!-- ralphus:exhibit:start action={} slot={} -->",
        key.action_idx, key.slot_idx
    )
}

/// The marker that closes one slot's block.
#[must_use]
pub fn slot_end(key: SlotKey) -> String {
    format!(
        "<!-- ralphus:exhibit:end action={} slot={} -->",
        key.action_idx, key.slot_idx
    )
}

/// The common prefix of every slot start marker.
const ANY_SLOT_START: &str = "<!-- ralphus:exhibit:start ";

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fenced(text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let clipped: String = text.chars().take(OUTPUT_MAX_CHARS).collect();
    let more = if text.chars().count() > OUTPUT_MAX_CHARS {
        "\n…"
    } else {
        ""
    };
    format!("{fence}text\n{}{more}\n{fence}", clipped.trim_end())
}

/// Markdown for an uploaded file. GitHub plays a video whose URL stands alone
/// on its line; GitLab plays one written as an image.
fn media(kind: ExhibitKind, url: &str, caption: &str, forge: crate::forge::ForgeKind) -> String {
    let alt = one_line(caption).replace(['[', ']'], "");
    match (kind, forge) {
        (ExhibitKind::Video, crate::forge::ForgeKind::GitHub) => url.to_string(),
        _ => format!("![{alt}]({url})"),
    }
}

/// One slot's block, markers included.
#[must_use]
pub fn render_slot(slot: &ExhibitSlot, action_label: &str, forge: crate::forge::ForgeKind) -> String {
    let requested = slot.request.kind;
    let describe = one_line(&slot.request.describe);
    let source = if action_label.trim().is_empty() {
        String::new()
    } else {
        format!(" _(live action: {})_", one_line(action_label))
    };
    let mut out = slot_start(slot.key);
    out.push('\n');
    let delivered = slot.fill.as_ref().filter(|f| f.url.is_some() || f.text.is_some());
    match delivered {
        Some(fill) => {
            let kind = fill.delivered_kind.unwrap_or(requested);
            let note = if kind == requested {
                String::new()
            } else {
                format!(" (delivered as {})", kind.display().to_lowercase())
            };
            out.push_str(&format!(
                "- [x] **{}**{note} — {describe}{source}\n",
                requested.display()
            ));
            let caption = fill.caption.as_deref().unwrap_or(&describe);
            if let Some(url) = &fill.url {
                if !caption.trim().is_empty() {
                    out.push_str(&format!("\n{}\n", one_line(caption)));
                }
                out.push_str(&format!("\n{}\n", media(kind, url, caption, forge)));
            } else if let Some(text) = &fill.text {
                out.push_str(&format!("\n{}\n", fenced(text)));
            }
        }
        None => {
            out.push_str(&format!(
                "- [ ] **{}** — {describe}{source}\n",
                requested.display()
            ));
            if let Some(fill) = &slot.fill {
                match (fill.local_path.is_some(), fill.missing_reason.as_deref()) {
                    (true, reason) => out.push_str(&format!(
                        "  _Captured, not uploaded yet{}; the file is on the ralphus board._\n",
                        reason.map(|r| format!(" ({})", one_line(r))).unwrap_or_default()
                    )),
                    (false, Some(reason)) => {
                        out.push_str(&format!("  _Not captured: {}_\n", one_line(reason)));
                    }
                    (false, None) => {}
                }
            }
        }
    }
    out.push_str(&slot_end(slot.key));
    out
}

/// The whole managed section for `blocks`, markers included.
#[must_use]
pub fn render_section(blocks: &[String]) -> String {
    let mut out = format!(
        "{SECTION_START}\n## Exhibits\n\n_Captured by ralphus live actions. Unchecked items are still to come._\n\n"
    );
    out.push_str(&blocks.join("\n\n"));
    out.push('\n');
    out.push_str(SECTION_END);
    out
}

/// The result of [`splice_exhibits`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spliced {
    pub body: String,
    /// Slots that were not written because their markers in the description
    /// are damaged (a start with no matching end, or a section with no end).
    pub damaged: Vec<SlotKey>,
}

/// Write `blocks` (each a [`render_slot`] block for its key) into `current`.
///
/// A slot whose block already exists anywhere in the description has exactly
/// the text between its markers replaced; nothing else moves. Slots that are
/// not there yet are appended to the end of the managed section, which is
/// created (before the live-test results section, if there is one) when it
/// does not exist. Slots not passed in are left alone, whatever their state.
/// Damaged markers are never "repaired": the slot is skipped and reported.
#[must_use]
pub fn splice_exhibits(current: &str, blocks: &[(SlotKey, String)]) -> Spliced {
    let mut body = current.to_string();
    let mut damaged = Vec::new();
    let mut new_blocks: Vec<&str> = Vec::new();
    for (key, block) in blocks {
        let start_marker = slot_start(*key);
        let end_marker = slot_end(*key);
        let Some(start) = body.find(&start_marker) else {
            new_blocks.push(block);
            continue;
        };
        let after = start + start_marker.len();
        let end = body[after..].find(&end_marker).map(|i| after + i);
        let next_start = body[after..].find(ANY_SLOT_START).map(|i| after + i);
        match end {
            Some(end) if next_start.is_none_or(|n| n > end) => {
                let end = end + end_marker.len();
                body.replace_range(start..end, block);
            }
            _ => damaged.push(*key),
        }
    }
    if new_blocks.is_empty() {
        return Spliced { body, damaged };
    }
    let added = new_blocks.join("\n\n");
    if let Some(section_start) = body.find(SECTION_START) {
        match body[section_start..].find(SECTION_END) {
            Some(rel) => {
                let end = section_start + rel;
                let head = body[..end].trim_end_matches(['\n', ' ']).to_string();
                body = format!("{head}\n\n{added}\n{}", &body[end..]);
            }
            None => {
                damaged.extend(
                    blocks
                        .iter()
                        .filter(|(_, b)| new_blocks.contains(&b.as_str()))
                        .map(|(k, _)| *k),
                );
            }
        }
        return Spliced { body, damaged };
    }
    let section = render_section(
        &new_blocks
            .iter()
            .map(|b| (*b).to_string())
            .collect::<Vec<_>>(),
    );
    body = match body.find(crate::live_results::SECTION_START) {
        Some(live) => {
            let head = body[..live].trim_end();
            let sep = if head.is_empty() { "" } else { "\n\n" };
            format!("{head}{sep}{section}\n\n{}", &body[live..])
        }
        None => {
            let base = body.trim_end();
            if base.is_empty() {
                section
            } else {
                format!("{base}\n\n{section}")
            }
        }
    };
    Spliced { body, damaged }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::ForgeKind;

    fn req(kind: ExhibitKind, describe: &str) -> ExhibitRequest {
        ExhibitRequest {
            kind,
            describe: describe.to_string(),
        }
    }

    fn slot(action: i64, idx: i64, kind: ExhibitKind, describe: &str) -> ExhibitSlot {
        ExhibitSlot {
            key: SlotKey {
                action_idx: action,
                slot_idx: idx,
            },
            request: req(kind, describe),
            origin: ExhibitOrigin::Handoff,
            why: None,
            fill: None,
        }
    }

    fn filled(mut s: ExhibitSlot, url: &str) -> ExhibitSlot {
        s.fill = Some(ExhibitFill {
            url: Some(url.to_string()),
            caption: Some("look at the points".to_string()),
            updated_at_ms: 1,
            ..ExhibitFill::default()
        });
        s
    }

    fn block(s: &ExhibitSlot) -> (SlotKey, String) {
        (s.key, render_slot(s, "Scatter slider", ForgeKind::GitHub))
    }

    #[test]
    fn split_handoff_lifts_exhibit_lines_out_of_the_steps() {
        let text = "Scatter density slider\nOpen test.hip and cook scatter1.\n  EXHIBIT: gif | drag density 0 to 1\nEXHIBIT: Screenshot | viewport at 0.5\nCheck the point count drops.";
        let split = split_handoff(text);
        assert_eq!(
            split.text,
            "Scatter density slider\nOpen test.hip and cook scatter1.\nCheck the point count drops."
        );
        assert_eq!(
            split.requests,
            vec![
                req(ExhibitKind::Gif, "drag density 0 to 1"),
                req(ExhibitKind::Screenshot, "viewport at 0.5"),
            ]
        );
        assert!(split.rejected.is_empty());
    }

    #[test]
    fn split_handoff_rejects_unknown_kinds_empty_descriptions_and_overflow() {
        let mut text = String::from("steps\nEXHIBIT: hologram | spin it\nEXHIBIT: gif |   \nEXHIBIT: gif no pipe");
        for i in 0..6 {
            text.push_str(&format!("\nEXHIBIT: screenshot | view {i}"));
        }
        let split = split_handoff(&text);
        assert_eq!(split.text, "steps");
        assert_eq!(split.requests.len(), MAX_PER_ACTION);
        assert_eq!(split.rejected.len(), 3 + 2);
    }

    #[test]
    fn split_handoff_drops_duplicate_requests_within_one_handoff() {
        let split = split_handoff("EXHIBIT: gif | Drag  the slider\nEXHIBIT: GIF | drag the slider");
        assert_eq!(split.requests.len(), 1);
    }

    #[test]
    fn merge_requests_keeps_order_drops_duplicates_and_caps() {
        let a = vec![req(ExhibitKind::Gif, "one"), req(ExhibitKind::Screenshot, "two")];
        let b = vec![
            req(ExhibitKind::Gif, "ONE"),
            req(ExhibitKind::Video, "three"),
            req(ExhibitKind::Output, "four"),
            req(ExhibitKind::Output, "five"),
        ];
        let merged = merge_requests([a.as_slice(), b.as_slice()]);
        assert_eq!(
            merged.iter().map(|r| r.describe.as_str()).collect::<Vec<_>>(),
            vec!["one", "two", "three", "four"]
        );
    }

    #[test]
    fn kinds_round_trip_and_only_output_is_not_visual() {
        for kind in ExhibitKind::ALL {
            assert_eq!(ExhibitKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ExhibitKind::parse(" GIF "), Some(ExhibitKind::Gif));
        assert!(!ExhibitKind::Output.is_visual());
        assert!(ExhibitKind::Video.is_visual());
    }

    #[test]
    fn an_unfilled_slot_renders_as_an_open_todo() {
        let s = slot(7, 1, ExhibitKind::Gif, "drag density 0→1");
        let text = render_slot(&s, "Scatter slider", ForgeKind::GitHub);
        assert_eq!(
            text,
            "<!-- ralphus:exhibit:start action=7 slot=1 -->\n- [ ] **GIF** — drag density 0→1 _(live action: Scatter slider)_\n<!-- ralphus:exhibit:end action=7 slot=1 -->"
        );
    }

    #[test]
    fn a_filled_video_is_a_bare_url_on_github_and_an_image_link_on_gitlab() {
        let s = filled(slot(0, 0, ExhibitKind::Video, "the slider"), "https://x/v.mp4");
        let gh = render_slot(&s, "", ForgeKind::GitHub);
        assert!(gh.contains("- [x] **Video** — the slider\n"));
        assert!(gh.contains("\nhttps://x/v.mp4\n"));
        let gl = render_slot(&s, "", ForgeKind::GitLab);
        assert!(gl.contains("![look at the points](https://x/v.mp4)"));
    }

    #[test]
    fn a_lower_delivered_kind_is_called_out() {
        let mut s = filled(slot(0, 0, ExhibitKind::Gif, "the slider"), "https://x/a.png");
        s.fill.as_mut().unwrap().delivered_kind = Some(ExhibitKind::Screenshot);
        let text = render_slot(&s, "", ForgeKind::GitHub);
        assert!(text.contains("- [x] **GIF** (delivered as screenshot) — the slider"));
        assert!(text.contains("![look at the points](https://x/a.png)"));
    }

    #[test]
    fn a_missing_slot_stays_open_with_its_reason() {
        let mut s = slot(0, 0, ExhibitKind::Gif, "the slider");
        s.fill = Some(ExhibitFill {
            missing_reason: Some("no display on the review machine".to_string()),
            updated_at_ms: 1,
            ..ExhibitFill::default()
        });
        let text = render_slot(&s, "", ForgeKind::GitHub);
        assert!(text.contains("- [ ] **GIF** — the slider\n  _Not captured: no display on the review machine_\n"));
    }

    #[test]
    fn output_is_fenced_with_a_fence_longer_than_any_backtick_run() {
        let mut s = slot(0, 0, ExhibitKind::Output, "the cli");
        s.fill = Some(ExhibitFill {
            text: Some("a ```` b".to_string()),
            updated_at_ms: 1,
            ..ExhibitFill::default()
        });
        let text = render_slot(&s, "", ForgeKind::GitHub);
        assert!(text.contains("`````text\na ```` b\n`````"));
    }

    #[test]
    fn splice_creates_the_section_before_live_results_and_keeps_other_text() {
        let a = slot(0, 0, ExhibitKind::Gif, "one");
        let current = format!(
            "Intro text.\n\n{}\n## Live-Test Results\n{}",
            crate::live_results::SECTION_START,
            crate::live_results::SECTION_END
        );
        let out = splice_exhibits(&current, &[block(&a)]);
        assert!(out.damaged.is_empty());
        let intro = out.body.find("Intro text.").unwrap();
        let exhibits = out.body.find(SECTION_START).unwrap();
        let live = out.body.find(crate::live_results::SECTION_START).unwrap();
        assert!(intro < exhibits && exhibits < live);
        assert!(out.body.contains(&slot_start(a.key)));
    }

    #[test]
    fn splice_into_an_empty_body_is_just_the_section() {
        let a = slot(0, 0, ExhibitKind::Gif, "one");
        let out = splice_exhibits("", &[block(&a)]);
        assert!(out.body.starts_with(SECTION_START));
        assert!(out.body.ends_with(SECTION_END));
    }

    #[test]
    fn a_rerun_replaces_only_its_own_slot_and_leaves_every_other_byte_alone() {
        let a = slot(7, 1, ExhibitKind::Gif, "one");
        let b = slot(7, 2, ExhibitKind::Screenshot, "two");
        let c = slot(9, 0, ExhibitKind::Video, "three");
        let first = splice_exhibits("Intro", &[block(&a), block(&b), block(&c)]).body;
        // A human note between slots must survive.
        let edited = first.replace(&slot_end(b.key), &format!("{}\n\nReviewer note.", slot_end(b.key)));

        let a2 = filled(a.clone(), "https://x/new.gif");
        let second = splice_exhibits(&edited, &[block(&a2)]);
        assert!(second.damaged.is_empty());
        let expected = edited.replace(&block(&a).1, &block(&a2).1);
        assert_eq!(second.body, expected);

        // Filling it again with another file replaces, never appends.
        let a3 = filled(a, "https://x/newer.gif");
        let third = splice_exhibits(&second.body, &[block(&a3)]).body;
        assert_eq!(third.matches(&slot_start(a3.key)).count(), 1);
        assert!(third.contains("newer.gif") && !third.contains("new.gif)"));
        assert!(third.contains("Reviewer note."));
    }

    #[test]
    fn splicing_the_same_blocks_twice_is_a_no_op() {
        let a = slot(0, 0, ExhibitKind::Gif, "one");
        let b = slot(1, 0, ExhibitKind::Output, "two");
        let once = splice_exhibits("Intro", &[block(&a), block(&b)]).body;
        let twice = splice_exhibits(&once, &[block(&a), block(&b)]).body;
        assert_eq!(once, twice);
    }

    #[test]
    fn a_new_slot_is_appended_inside_the_existing_section() {
        let a = slot(0, 0, ExhibitKind::Gif, "one");
        let b = slot(0, 1, ExhibitKind::Screenshot, "two");
        let once = splice_exhibits("Intro\n\nOutro", &[block(&a)]).body;
        let twice = splice_exhibits(&once, &[block(&a), block(&b)]).body;
        assert_eq!(twice.matches(SECTION_START).count(), 1);
        let b_at = twice.find(&slot_start(b.key)).unwrap();
        assert!(twice.find(&slot_end(a.key)).unwrap() < b_at);
        assert!(b_at < twice.find(SECTION_END).unwrap());
    }

    #[test]
    fn slots_not_passed_in_are_never_touched() {
        let a = slot(0, 0, ExhibitKind::Gif, "one");
        let gone = slot(5, 0, ExhibitKind::Gif, "action no longer in the review");
        let body = splice_exhibits("", &[block(&a), block(&gone)]).body;
        let a2 = filled(a, "https://x/a.gif");
        let out = splice_exhibits(&body, &[block(&a2)]).body;
        assert!(out.contains(&block(&gone).1));
    }

    #[test]
    fn damaged_markers_are_reported_and_never_written() {
        let a = slot(0, 0, ExhibitKind::Gif, "one");
        let b = slot(0, 1, ExhibitKind::Gif, "two");
        let body = splice_exhibits("", &[block(&a), block(&b)]).body;
        // A human deleted slot 0's end marker.
        let broken = body.replacen(&slot_end(a.key), "", 1);
        let a2 = filled(a.clone(), "https://x/a.gif");
        let out = splice_exhibits(&broken, &[block(&a2)]);
        assert_eq!(out.damaged, vec![a.key]);
        assert_eq!(out.body, broken);
    }

    #[test]
    fn a_section_without_its_end_marker_gets_no_new_slots() {
        let a = slot(0, 0, ExhibitKind::Gif, "one");
        let broken = format!("Intro\n{SECTION_START}\n## Exhibits\n");
        let out = splice_exhibits(&broken, &[block(&a)]);
        assert_eq!(out.damaged, vec![a.key]);
        assert_eq!(out.body, broken);
    }

    #[test]
    fn a_pr_shows_only_its_branch_slots_and_the_cap_prefers_handoff_requests() {
        let mut slots = vec![
            slot(0, 0, ExhibitKind::Gif, "a0"),
            slot(0, 1, ExhibitKind::Gif, "a1"),
            slot(1, 0, ExhibitKind::Gif, "b0"),
            slot(2, 0, ExhibitKind::Gif, "unattributed"),
        ];
        slots[1].origin = ExhibitOrigin::Inferred;
        let attribution = |action: i64| match action {
            0 => vec!["br-a".to_string()],
            1 => vec!["br-b".to_string()],
            _ => Vec::new(),
        };
        let on_a = slots_for_pr(&slots, Some("br-a"), &attribution, 10);
        assert_eq!(
            on_a.iter().map(|s| s.request.describe.as_str()).collect::<Vec<_>>(),
            vec!["a0", "a1", "unattributed"]
        );
        let capped = slots_for_pr(&slots, Some("br-a"), &attribution, 2);
        assert_eq!(
            capped.iter().map(|s| s.request.describe.as_str()).collect::<Vec<_>>(),
            vec!["a0", "unattributed"]
        );
        assert_eq!(slots_for_pr(&slots, None, &attribution, 10).len(), 4);
    }

    #[test]
    fn a_run_only_fills_its_own_actions_slots_and_the_last_report_wins() {
        let slots = vec![slot(7, 0, ExhibitKind::Gif, "a"), slot(7, 1, ExhibitKind::Output, "b")];
        let reply = "report\n\
            RALPHUS_EXHIBIT: {\"slot\": \"7.0\", \"file\": \"first.gif\"}\n\
            RALPHUS_EXHIBIT: {\"slot\": \"7.0\", \"file\": \"better.gif\", \"caption\": \"the points thin out\"}\n\
            RALPHUS_EXHIBIT: {\"slot\": \"7.1\", \"missing\": \"the CLI  crashed\"}\n\
            RALPHUS_EXHIBIT: {\"slot\": \"9.0\", \"file\": \"steal.gif\"}\n\
            RALPHUS_EXHIBIT: {\"slot\": \"7.5\", \"file\": \"nope.gif\"}\n\
            RALPHUS_EXHIBIT: not json\n\
            RALPHUS_LIVE_RESULT: {}";
        let (kept, ignored) = parse_reports(reply, 7, &slots);
        assert_eq!(
            kept,
            vec![
                SlotReport::File {
                    key: slots[0].key,
                    file: "better.gif".into(),
                    caption: Some("the points thin out".into()),
                },
                SlotReport::Missing {
                    key: slots[1].key,
                    reason: "the CLI crashed".into(),
                },
            ]
        );
        assert_eq!(ignored.len(), 3, "another action's slot, an unknown slot, and junk");
    }

    #[test]
    fn a_reported_file_must_sit_directly_in_the_run_directory() {
        let dir = crate::exhibit_media::tests::TestDir::new();
        std::fs::write(dir.path().join("ok.png"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("deep.png"), b"x").unwrap();
        assert!(resolve_reported_file(dir.path(), "ok.png").is_some());
        assert!(resolve_reported_file(dir.path(), "sub/deep.png").is_none());
        assert!(resolve_reported_file(dir.path(), "../ok.png").is_none());
        assert!(resolve_reported_file(dir.path(), "missing.png").is_none());
        assert!(resolve_reported_file(dir.path(), "sub").is_none(), "a directory is not a file");
    }

    #[test]
    fn the_run_prompt_names_every_slot_the_order_and_the_report_format() {
        let slots = vec![slot(3, 0, ExhibitKind::Gif, "drag it"), slot(3, 1, ExhibitKind::Output, "the log")];
        let settings = ExhibitSettings {
            kind_preference: vec![ExhibitKind::Video, ExhibitKind::Gif],
            max_video_seconds: 9,
            ..ExhibitSettings::default()
        };
        let text = run_prompt_section(&slots, &settings, std::path::Path::new("/tmp/ex"));
        assert!(text.contains("- 3.0 (gif): drag it"));
        assert!(text.contains("- 3.1 (output): the log"));
        assert!(text.contains("video, then gif"));
        assert!(text.contains("under 9 seconds"));
        assert!(text.contains(REPORT_MARKER));
        let follow = followup_prompt(&slots[..1], &settings, std::path::Path::new("/tmp/ex"), Some("it worked"));
        assert!(follow.contains("still missing") && follow.contains("it worked") && !follow.contains("3.1"));
    }

    #[test]
    fn inferred_requests_are_read_before_the_description_only() {
        let reply = "TITLE: x\n\
            EXHIBIT_REQUEST: {\"owner\": 2, \"kind\": \"gif\", \"describe\": \"drag the slider\", \"why\": \"it moves\"}\n\
            EXHIBIT_REQUEST: {\"owner\": \"new\", \"kind\": \"screenshot\", \"describe\": \"the panel\"}\n\
            EXHIBIT_REQUEST: {\"owner\": \"3\", \"kind\": \"hologram\", \"describe\": \"no\"}\n\
            EXHIBIT_REQUEST: {\"owner\": 2, \"kind\": \"GIF\", \"describe\": \"Drag the slider\"}\n\
            ===DESCRIPTION===\n\
            EXHIBIT_REQUEST: {\"owner\": 1, \"kind\": \"gif\", \"describe\": \"in the body, ignored\"}";
        let got = parse_inferred(reply);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].owner, InferredOwner::Action(2));
        assert_eq!(got[0].why.as_deref(), Some("it moves"));
        assert_eq!(got[1].owner, InferredOwner::New);
        assert_eq!(got[1].request.kind, ExhibitKind::Screenshot);
    }

    fn ask(describe: &str, owner: InferredOwner) -> InferredRequest {
        InferredRequest {
            owner,
            request: req(ExhibitKind::Screenshot, describe),
            why: Some("visible".into()),
        }
    }

    fn live_action(store: &crate::store::Store, g: &str, idx: i64, branch: Option<&str>) {
        store
            .conn
            .execute(
                "INSERT INTO guardian_live_actions (guardian_id, idx, label, prompt, source_cells, created_at_ms)
                 VALUES (?, ?, 'A', 'test', '[]', 0)",
                rusqlite::params![g, idx],
            )
            .unwrap();
        if let Some(branch) = branch {
            store
                .conn
                .execute(
                    "INSERT INTO guardian_live_action_branches (guardian_id, action_idx, branch_id) VALUES (?, ?, ?)",
                    rusqlite::params![g, idx, branch],
                )
                .unwrap();
        }
    }

    #[test]
    fn an_inferred_request_joins_the_named_action_only_when_it_covers_the_branch() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let g = store.create_guardian("r", "main", "/repo").unwrap();
        live_action(&store, &g, 0, Some("br-a"));
        live_action(&store, &g, 1, Some("br-b"));
        let attached = store
            .apply_inferred_exhibits(
                &g,
                "br-a",
                &[
                    ask("named and covering", InferredOwner::Action(0)),
                    // Action 1 is another branch's: falls back to br-a's action.
                    ask("named but elsewhere", InferredOwner::Action(1)),
                    ask("asked for new", InferredOwner::New),
                ],
            )
            .unwrap();
        assert_eq!(attached, 3);
        let slots = store.guardian_exhibit_slots(&g).unwrap();
        assert!(slots.iter().all(|s| s.key.action_idx == 0 && s.origin == ExhibitOrigin::Inferred));
        assert_eq!(slots[0].why.as_deref(), Some("visible"));
    }

    #[test]
    fn inferred_requests_wait_until_live_actions_are_written() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let g = store.create_guardian("r", "main", "/repo").unwrap();
        let now = store
            .apply_inferred_exhibits(&g, "br-a", &[ask("a panel", InferredOwner::Action(0))])
            .unwrap();
        assert_eq!(now, 0);
        assert!(store.guardian_exhibit_slots(&g).unwrap().is_empty());
        // Writing the review's live actions assigns what waited: no action
        // covers br-a, so it gets an exhibits action of its own.
        store
            .insert_guardian_live_actions(
                &g,
                &[crate::live_actions::NewLiveAction {
                    label: "A".into(),
                    prompt: "test a".into(),
                    source_cells: vec![],
                    exhibits: vec![req(ExhibitKind::Gif, "a moving")],
                }],
            )
            .unwrap();
        let actions = store.guardian_live_actions(&g).unwrap();
        assert_eq!(actions.len(), 2);
        let slots = store.guardian_exhibit_slots(&g).unwrap();
        assert!(slots.iter().any(|s| s.key.action_idx == 0 && s.request.describe == "a moving"));
        assert!(slots.iter().any(|s| s.key.action_idx == 1 && s.request.describe == "a panel"));
        let pending: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM guardian_pending_exhibits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn a_branch_without_any_covering_action_gets_an_exhibits_action_of_its_own() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let g = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO guardian_pending_exhibits (guardian_id, branch_id, kind, describe, why, created_at_ms)
                 VALUES (?, 'br-z', 'gif', 'z moving', NULL, 0)",
                rusqlite::params![g],
            )
            .unwrap();
        // An existing action attributed only to another branch.
        store
            .conn
            .execute(
                "INSERT INTO guardian_live_actions (guardian_id, idx, label, prompt, source_cells, created_at_ms)
                 VALUES (?, 0, 'A', 'test a', '[]', 0)",
                rusqlite::params![g],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO guardian_live_action_branches (guardian_id, action_idx, branch_id) VALUES (?, 0, 'br-a')",
                rusqlite::params![g],
            )
            .unwrap();
        store.assign_pending_exhibits(&g).unwrap();
        let actions = store.guardian_live_actions(&g).unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[1].idx, 1);
        assert!(actions[1].label.starts_with("Exhibits: "));
        assert_eq!(store.live_action_branch_ids(&g, 1).unwrap(), vec!["br-z".to_string()]);
        let slots = store.guardian_exhibit_slots(&g).unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].key.action_idx, 1);
    }

    #[test]
    fn stored_requests_dedupe_cap_and_fill_replacement_round_trip() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let g = store.create_guardian("r", "main", "/repo").unwrap();
        let g = g.as_str();
        let created = store
            .add_exhibit_requests(
                g,
                3,
                ExhibitOrigin::Handoff,
                &[
                    (req(ExhibitKind::Gif, "one"), None),
                    (req(ExhibitKind::Gif, "ONE"), None),
                    (req(ExhibitKind::Screenshot, "two"), None),
                ],
            )
            .unwrap();
        assert_eq!(created.len(), 2);
        let more = store
            .add_exhibit_requests(
                g,
                3,
                ExhibitOrigin::Inferred,
                &[
                    (req(ExhibitKind::Video, "three"), Some("motion".to_string())),
                    (req(ExhibitKind::Output, "four"), None),
                    (req(ExhibitKind::Output, "five"), None),
                ],
            )
            .unwrap();
        assert_eq!(more.len(), MAX_PER_ACTION - 2);
        let slots = store.guardian_exhibit_slots(g).unwrap();
        assert_eq!(slots.len(), MAX_PER_ACTION);
        assert_eq!(slots[2].origin, ExhibitOrigin::Inferred);
        assert_eq!(slots[2].why.as_deref(), Some("motion"));

        let key = slots[0].key;
        let first = ExhibitFill {
            url: Some("https://x/1.gif".to_string()),
            forge_upload_id: Some("11".to_string()),
            updated_at_ms: 1,
            ..ExhibitFill::default()
        };
        assert_eq!(store.set_exhibit_fill(g, key, &first).unwrap(), None);
        let second = ExhibitFill {
            url: Some("https://x/2.gif".to_string()),
            forge_upload_id: Some("12".to_string()),
            updated_at_ms: 2,
            ..ExhibitFill::default()
        };
        let replaced = store.set_exhibit_fill(g, key, &second).unwrap();
        assert_eq!(replaced.and_then(|f| f.forge_upload_id).as_deref(), Some("11"));
        let now = store.guardian_exhibit_slots(g).unwrap();
        let fill = now[0].fill.clone().unwrap();
        assert_eq!(fill.url.as_deref(), Some("https://x/2.gif"));
        // The replaced upload waits for deletion; the live one never does.
        assert_eq!(fill.stale_upload_ids, vec!["11".to_string()]);

        // A third fill before the second was cleaned up keeps both pending.
        let third = ExhibitFill {
            local_path: Some("/state/3.gif".to_string()),
            updated_at_ms: 3,
            ..ExhibitFill::default()
        };
        store.set_exhibit_fill(g, key, &third).unwrap();
        let fill = store.guardian_exhibit_slots(g).unwrap()[0].fill.clone().unwrap();
        assert_eq!(fill.stale_upload_ids, vec!["11".to_string(), "12".to_string()]);
        assert_eq!(fill.url, None);
        store
            .record_exhibit_upload(g, key, "https://x/3.gif", Some("13"))
            .unwrap();
        store
            .clear_stale_exhibit_uploads(g, key, &["11".to_string()])
            .unwrap();
        let fill = store.guardian_exhibit_slots(g).unwrap()[0].fill.clone().unwrap();
        assert_eq!(fill.url.as_deref(), Some("https://x/3.gif"));
        assert_eq!(fill.forge_upload_id.as_deref(), Some("13"));
        assert_eq!(fill.stale_upload_ids, vec!["12".to_string()]);
        assert!(store.guardian_has_exhibits(g).unwrap());
        assert!(!store.guardian_has_exhibits("other").unwrap());
    }
}
