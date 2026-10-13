//! Post-merge follow-up offers: once a review merges, the `deferred`
//! prophecies its cells wrote can become a follow-up squad.
//!
//! The flow, end to end:
//!
//! 1. [`Store::maybe_offer_followups`] runs when a review reaches `merged`
//!    (`set_guardian_status`). It snapshots the review's `deferred` prophecies
//!    -- with the prompt of the cell that wrote each -- into one
//!    `followup_offers` row and sends one mailbox message. The row's primary
//!    key is the review id, so a review offers at most once however often it
//!    is reopened and re-merged.
//! 2. The user accepts or declines (`POST /api/guardians/{id}/followup/...`,
//!    `ralphus review followup ...`).
//! 3. [`accept_offer`] drafts one squad (one task per deferred item) and
//!    submits it through the ordinary submit path, and records the squad
//!    against each item and the offer. No waypoint is created.
//!
//! Drafting is deterministic: each task's prompt is the deferred note plus
//! the original prompt as context. There is no model call, so drafting has no
//! failure or cost mode of its own, and the whole flow runs without an agent.
//!
//! The squad's base branch is the branch the review merged into, or the
//! remote's default branch when that branch no longer exists.

use std::path::Path;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use ralphus_core::schema::UpstreamRef;

use crate::guardian::GuardianView;
use crate::store::{Result, Store, StoreError, now_ms};

/// The offer has been sent and awaits an answer.
pub const STATUS_OFFERED: &str = "offered";
/// The user accepted; the squad was created.
pub const STATUS_ACCEPTED: &str = "accepted";
/// The user declined; nothing was created.
pub const STATUS_DECLINED: &str = "declined";

/// Longest original prompt kept in an offer's snapshot, in characters. The
/// snapshot is context for the follow-up agent, not an archive.
const MAX_PROMPT_CHARS: usize = 4000;

/// Longest deferred note shown in the offer message, in characters.
const MAX_MESSAGE_NOTE_CHARS: usize = 240;

/// One deferred prophecy in an offer, with the cell context it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowupItem {
    /// The `prophecies` row this item snapshots.
    pub prophecy_id: i64,
    /// Owner of the prophecy (`cell:...` or `guardian:...`).
    pub entity_uri: String,
    /// The deferred note, verbatim.
    pub body: String,
    /// The prompt of the cell that wrote it, truncated; `None` when the
    /// owner was not a prompt cell or its squad is already gone.
    #[serde(default)]
    pub prompt: Option<String>,
    /// The agent that cell ran, so the follow-up starts on the same one.
    #[serde(default)]
    pub agent: Option<String>,
    /// The model that cell ran.
    #[serde(default)]
    pub model: Option<String>,
}

/// What the cell that wrote a deferral was running, as far as it still exists.
struct CellContext {
    prompt: Option<String>,
    agent: String,
    model: Option<String>,
    branch: Option<String>,
    task_name: String,
}

/// One row of `followup_offers`.
#[derive(Debug, Clone, Serialize)]
pub struct FollowupOfferView {
    pub guardian_id: String,
    pub status: String,
    pub items: Vec<FollowupItem>,
    /// Follow-up generation of the review that triggered the offer (`0` for
    /// ordinary work).
    pub depth: i64,
    /// The squad an accepted offer created.
    pub squad_id: Option<String>,
    pub created_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
}

const OFFER_COLUMNS: &str =
    "guardian_id, status, items, depth, squad_id, created_at_ms, resolved_at_ms";

fn map_offer_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FollowupOfferView> {
    let items: String = r.get(2)?;
    Ok(FollowupOfferView {
        guardian_id: r.get(0)?,
        status: r.get(1)?,
        // A malformed snapshot reads as empty rather than failing the whole
        // lookup: the offer row still has to be answerable (declinable).
        items: serde_json::from_str(&items).unwrap_or_default(),
        depth: r.get(3)?,
        squad_id: r.get(4)?,
        created_at_ms: r.get(5)?,
        resolved_at_ms: r.get(6)?,
    })
}

/// One deferred prophecy a review has pending, with whether the user ignored
/// it. Only the latest attempt of each cell is listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingFollowupItem {
    #[serde(flatten)]
    pub item: FollowupItem,
    /// The attempt of the writing cell this deferral came from.
    pub attempt: i64,
    /// Whether the user ignored it; ignored items are left out of the offer.
    pub ignored: bool,
    pub created_at_ms: i64,
    /// The review branch the writing cell belongs to, so the board can name
    /// where a follow-up came from rather than showing the cell's URI.
    pub branch: Option<String>,
    /// The name of the task the writing cell belongs to.
    pub task_name: Option<String>,
    /// The follow-up squad accepting this item created; `None` until it is
    /// accepted.
    pub squad_id: Option<String>,
    /// Whether the user accepted this item: its squad exists, or it was
    /// accepted ahead of the merge and is submitted when the offer is made.
    pub accepted: bool,
}

/// A review's pending follow-ups: what the board's button shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingFollowupsSummary {
    /// Whether the review would offer follow-up at merge: it is enabled for
    /// the review and no offer was made yet.
    pub enabled: bool,
    /// The review's follow-up setting resolved to off.
    pub off: bool,
    /// An offer row already exists; the snapshot is final.
    pub offered: bool,
    /// Deferrals not ignored.
    pub count: usize,
    /// Every deferral, ignored or not.
    pub total: usize,
}

/// The `GET /api/guardians/{id}/followup/items` body.
#[derive(Debug, Clone, Serialize)]
pub struct PendingFollowups {
    #[serde(flatten)]
    pub summary: PendingFollowupsSummary,
    pub items: Vec<PendingFollowupItem>,
    /// The merge-time offer's status (`offered`/`accepted`/`declined`), or
    /// `None` before any offer exists.
    pub offer_status: Option<String>,
    /// The squad an accepted offer created for these items; `None` while the
    /// offer is unanswered, declined, or its squad was not created.
    pub offer_squad_id: Option<String>,
}

fn summarize_pending(
    items: &[PendingFollowupItem],
    enabled_for_review: bool,
    offered: bool,
) -> PendingFollowupsSummary {
    PendingFollowupsSummary {
        enabled: enabled_for_review && !offered,
        off: !enabled_for_review,
        offered,
        count: items.iter().filter(|i| !i.ignored).count(),
        total: items.len(),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

impl Store {
    /// The offer recorded for `guardian_id`, if one was ever sent.
    pub fn get_followup_offer(&self, guardian_id: &str) -> Result<Option<FollowupOfferView>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {OFFER_COLUMNS} FROM followup_offers WHERE guardian_id=?"),
                params![guardian_id],
                map_offer_row,
            )
            .optional()?)
    }

    /// The follow-up generation of `guardian_id`: `0` for ordinary work, and
    /// one more than the offer's own generation when the review's squad was
    /// itself created by accepting an offer. Compared against
    /// `FollowupConfig::max_depth` so a follow-up's deferrals do not chain.
    pub fn followup_depth_of_review(&self, guardian_id: &str) -> Result<i64> {
        let depth: Option<i64> = self.conn.query_row(
            "SELECT MAX(o.depth) + 1 FROM followup_offers o
             WHERE (o.squad_id IS NOT NULL AND (
                 o.squad_id IN (SELECT squad_id FROM guardians WHERE id=?1)
                 OR o.squad_id IN (SELECT squad_id FROM cells WHERE review_guardian_id=?1)
             )) OR o.guardian_id IN (
                 SELECT r.guardian_id FROM followup_item_results r
                 WHERE r.squad_id IN (SELECT squad_id FROM guardians WHERE id=?1)
                    OR r.squad_id IN (SELECT squad_id FROM cells WHERE review_guardian_id=?1)
             )",
            params![guardian_id],
            |r| r.get(0),
        )?;
        Ok(depth.unwrap_or(0))
    }

    /// The prompt, agent, model, review branch and task name of the cell
    /// named by a prophecy's `entity_uri`, when that cell still exists.
    fn cell_context_for_uri(&self, entity_uri: &str) -> Option<CellContext> {
        let crate::entity_uri::EntityUri::Cell {
            squad_id,
            task_idx,
            cell_idx,
        } = crate::entity_uri::parse(entity_uri)?
        else {
            return None;
        };
        let cell = self
            .cells_of(&squad_id)
            .ok()?
            .into_iter()
            .find(|c| c.task_idx == task_idx && c.idx == cell_idx)?;
        let branch = self
            .conn
            .query_row(
                "SELECT review_branch FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten();
        Some(CellContext {
            prompt: cell.prompt,
            agent: cell.agent,
            model: cell.model,
            branch,
            task_name: cell.task_name,
        })
    }

    /// The `deferred` prophecies `guardian_id`'s review would offer: for each
    /// owner (cell), only the deferrals of its latest attempt, oldest first,
    /// each flagged with whether the user ignored it.
    pub fn pending_followup_items(&self, guardian_id: &str) -> Result<Vec<PendingFollowupItem>> {
        let deferred: Vec<_> = self
            .list_all_prophecies_for_guardian(guardian_id)?
            .into_iter()
            .filter(|p| p.kind == crate::prophecy::ProphecyKind::Deferred.as_str())
            .collect();
        let mut latest: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for p in &deferred {
            let slot = latest.entry(p.entity_uri.clone()).or_insert(p.attempt);
            *slot = (*slot).max(p.attempt);
        }
        let ignored = self.ignored_followup_ids(guardian_id)?;
        let results = self.followup_item_squads(guardian_id)?;
        let preaccepted = self.preaccepted_followup_ids(guardian_id)?;
        // An offer accepted whole, before items were accepted one by one, has
        // no per-item rows: its single squad stands for every item it offered.
        let whole_offer_squad = self
            .get_followup_offer(guardian_id)?
            .filter(|o| o.status == STATUS_ACCEPTED)
            .map(|o| o.squad_id.filter(|s| !s.is_empty()));
        Ok(deferred
            .into_iter()
            .filter(|p| latest.get(&p.entity_uri) == Some(&p.attempt))
            .map(|p| {
                let context = self.cell_context_for_uri(&p.entity_uri);
                PendingFollowupItem {
                    ignored: ignored.contains(&p.id),
                    attempt: p.attempt,
                    created_at_ms: p.created_at_ms,
                    branch: context.as_ref().and_then(|c| c.branch.clone()),
                    task_name: context.as_ref().map(|c| c.task_name.clone()),
                    squad_id: results
                        .get(&p.id)
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .or_else(|| {
                            whole_offer_squad.clone().flatten().filter(|_| {
                                !results.contains_key(&p.id) && !ignored.contains(&p.id)
                            })
                        }),
                    accepted: results.contains_key(&p.id)
                        || preaccepted.contains(&p.id)
                        || (whole_offer_squad.is_some() && !ignored.contains(&p.id)),
                    item: FollowupItem {
                        prophecy_id: p.id,
                        prompt: context
                            .as_ref()
                            .and_then(|c| c.prompt.as_deref())
                            .map(|prompt| truncate_chars(prompt, MAX_PROMPT_CHARS)),
                        agent: context.as_ref().map(|c| c.agent.clone()),
                        model: context.as_ref().and_then(|c| c.model.clone()),
                        entity_uri: p.entity_uri,
                        body: p.body,
                    },
                }
            })
            .collect())
    }

    fn ignored_followup_ids(&self, guardian_id: &str) -> Result<std::collections::HashSet<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT prophecy_id FROM followup_ignored WHERE guardian_id=?")?;
        let ids = stmt
            .query_map(params![guardian_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(ids)
    }

    fn preaccepted_followup_ids(
        &self,
        guardian_id: &str,
    ) -> Result<std::collections::HashSet<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT prophecy_id FROM followup_preaccepted WHERE guardian_id=?")?;
        let ids = stmt
            .query_map(params![guardian_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(ids)
    }

    /// Accepts one pending deferral before the review has merged. The offer
    /// does not exist yet, so this only records the intent: when the merge
    /// makes the offer, [`spawn_preaccepted_worker`] submits the item at once.
    /// Accepting an ignored item un-ignores it. Refused once the offer exists
    /// (accept it through [`accept_offer`] then). An id that is not one of the
    /// review's pending deferrals is `NotFound`.
    pub fn preaccept_followup(&self, guardian_id: &str, prophecy_id: i64) -> Result<()> {
        if self.get_followup_offer(guardian_id)?.is_some() {
            return Err(StoreError::InvalidTransition(
                "this review's follow-up offer already exists; accept the item from the offer"
                    .to_string(),
            ));
        }
        let items = self.pending_followup_items(guardian_id)?;
        if !items.iter().any(|i| i.item.prophecy_id == prophecy_id) {
            return Err(StoreError::NotFound);
        }
        self.conn.execute(
            "DELETE FROM followup_ignored WHERE guardian_id=? AND prophecy_id=?",
            params![guardian_id, prophecy_id],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO followup_preaccepted(guardian_id, prophecy_id, created_at_ms)
             VALUES(?,?,?)",
            params![guardian_id, prophecy_id, now_ms()],
        )?;
        crate::cartographer::Note::new("followup")
            .scope("followup")
            .guardian(guardian_id)
            .emit(
                self,
                format!(
                    "review {guardian_id}: deferred follow-up (prophecy {prophecy_id}) accepted \
                     ahead of the merge"
                ),
                serde_json::json!({"prophecy_id": prophecy_id}),
            );
        Ok(())
    }

    /// Items accepted ahead of the merge whose offer now exists but whose
    /// squad does not: `(review id, prophecy id)`, oldest offer first.
    pub fn due_preaccepted_followups(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.guardian_id, p.prophecy_id FROM followup_preaccepted p
             JOIN followup_offers o ON o.guardian_id=p.guardian_id AND o.status=?
             LEFT JOIN followup_item_results r
                 ON r.guardian_id=p.guardian_id AND r.prophecy_id=p.prophecy_id
             WHERE r.prophecy_id IS NULL
             ORDER BY o.created_at_ms, p.prophecy_id",
        )?;
        let rows = stmt
            .query_map(params![STATUS_OFFERED], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// The items and button state of the review's pending follow-ups.
    pub fn pending_followups(&self, guardian_id: &str) -> Result<PendingFollowups> {
        let items = self.pending_followup_items(guardian_id)?;
        let summary = self.summarize_pending_for(guardian_id, &items)?;
        let offer = self.get_followup_offer(guardian_id)?;
        Ok(PendingFollowups {
            summary,
            items,
            offer_status: offer.as_ref().map(|o| o.status.clone()),
            offer_squad_id: offer.and_then(|o| o.squad_id).filter(|s| !s.is_empty()),
        })
    }

    /// Just the button state of the review's pending follow-ups.
    pub fn pending_followups_summary(&self, guardian_id: &str) -> Result<PendingFollowupsSummary> {
        let items = self.pending_followup_items(guardian_id)?;
        self.summarize_pending_for(guardian_id, &items)
    }

    fn summarize_pending_for(
        &self,
        guardian_id: &str,
        items: &[PendingFollowupItem],
    ) -> Result<PendingFollowupsSummary> {
        let guardian = self.get_guardian(guardian_id)?;
        Ok(summarize_pending(
            items,
            guardian.effective_followup_enabled,
            self.get_followup_offer(guardian_id)?.is_some(),
        ))
    }

    /// Ignores (`ignored`) or un-ignores one pending deferral, or every
    /// pending deferral when `prophecy_id` is `None`. Returns how many items
    /// changed state. Refused once the review's offer exists: the merge-time
    /// snapshot is final. An id that is not one of the review's pending
    /// deferrals is `NotFound`.
    pub fn set_followup_ignored(
        &self,
        guardian_id: &str,
        prophecy_id: Option<i64>,
        ignored: bool,
    ) -> Result<usize> {
        if self.get_followup_offer(guardian_id)?.is_some() {
            return Err(StoreError::InvalidTransition(
                "this review's follow-up offer already exists; its items are final".to_string(),
            ));
        }
        let items = self.pending_followup_items(guardian_id)?;
        let targets: Vec<&PendingFollowupItem> = match prophecy_id {
            Some(id) => vec![
                items
                    .iter()
                    .find(|i| i.item.prophecy_id == id)
                    .ok_or(StoreError::NotFound)?,
            ],
            None => items.iter().collect(),
        };
        let mut changed = 0;
        for item in targets.into_iter().filter(|i| i.ignored != ignored) {
            let id = item.item.prophecy_id;
            if ignored {
                self.conn.execute(
                    "INSERT OR IGNORE INTO followup_ignored(guardian_id, prophecy_id, created_at_ms)
                     VALUES(?,?,?)",
                    params![guardian_id, id, now_ms()],
                )?;
                self.conn.execute(
                    "DELETE FROM followup_preaccepted WHERE guardian_id=? AND prophecy_id=?",
                    params![guardian_id, id],
                )?;
            } else {
                self.conn.execute(
                    "DELETE FROM followup_ignored WHERE guardian_id=? AND prophecy_id=?",
                    params![guardian_id, id],
                )?;
            }
            changed += 1;
            let verb = if ignored { "ignored" } else { "un-ignored" };
            crate::cartographer::Note::new("followup")
                .scope("followup")
                .guardian(guardian_id)
                .emit(
                    self,
                    format!("review {guardian_id}: deferred follow-up (prophecy {id}) {verb}"),
                    serde_json::json!({"prophecy_id": id, "ignored": ignored}),
                );
        }
        Ok(changed)
    }

    /// Offers follow-up work for the `deferred` prophecies of a review that
    /// just merged. Returns whether an offer was sent.
    ///
    /// Sends nothing -- and records nothing -- when offers are off for this
    /// review (its own `followup_enabled` override, else the global
    /// `[followup] enabled`), when the `[followup]` config is invalid, when
    /// the review is already at the configured follow-up depth, or when it
    /// wrote no `deferred` prophecy. Once a row exists the review
    /// never offers again, so reopening it and re-merging is silent.
    pub fn maybe_offer_followups(&self, guardian_id: &str) -> Result<bool> {
        let global = crate::config::global_followup_config();
        let git_root = self.get_guardian(guardian_id)?.git_root;
        let config =
            crate::config::layered_followup_config(std::path::Path::new(&git_root), &global);
        self.maybe_offer_followups_with(guardian_id, &config)
    }

    /// [`Self::maybe_offer_followups`] against an explicit global `[followup]`
    /// config, so tests need not touch the process-wide config file.
    fn maybe_offer_followups_with(
        &self,
        guardian_id: &str,
        config: &crate::config::FollowupConfig,
    ) -> Result<bool> {
        if self.get_followup_offer(guardian_id)?.is_some() {
            return Ok(false);
        }
        let guardian = self.get_guardian(guardian_id)?;
        if !guardian.effective_followup_enabled {
            return Ok(false);
        }
        if let Err(error) = config.validate(true) {
            crate::cartographer::Note::new("followup")
                .level(crate::logging::LogLevel::WARNING)
                .scope("followup")
                .guardian(guardian_id)
                .emit(
                    self,
                    format!(
                        "review {guardian_id}: invalid [followup] config, no follow-up offered: \
                         {error}"
                    ),
                    serde_json::json!({"error": error}),
                );
            return Ok(false);
        }
        let depth = self.followup_depth_of_review(guardian_id)?;
        if depth >= i64::from(config.max_depth()) {
            crate::cartographer::Note::new("followup")
                .scope("followup")
                .guardian(guardian_id)
                .emit(
                    self,
                    format!(
                        "review {guardian_id} is follow-up generation {depth}, at the cap of {}: \
                         no follow-up offered",
                        config.max_depth()
                    ),
                    serde_json::json!({"depth": depth, "max_depth": config.max_depth()}),
                );
            return Ok(false);
        }
        let items: Vec<FollowupItem> = self
            .pending_followup_items(guardian_id)?
            .into_iter()
            .filter(|p| !p.ignored)
            .map(|p| p.item)
            .collect();
        if items.is_empty() {
            return Ok(false);
        }
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO followup_offers(guardian_id, status, items, depth, created_at_ms)
             VALUES(?,?,?,?,?)",
            params![
                guardian_id,
                STATUS_OFFERED,
                serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string()),
                depth,
                now_ms()
            ],
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        let message = offer_message(guardian_id, &guardian.name, &items);
        crate::cartographer::Note::new("followup")
            .scope("followup")
            .guardian(guardian_id)
            .emit(
                self,
                format!(
                    "review {guardian_id} merged with {} deferred item(s): follow-up offered",
                    items.len()
                ),
                serde_json::json!({"count": items.len(), "depth": depth}),
            );
        if let Err(e) = self.notify_watchers(
            crate::monitor::NotifiableEventKind::ReviewFollowupOffered,
            &format!("guardian:{guardian_id}"),
            crate::mailbox::MailboxPriority::Normal,
            &message,
            guardian.squad_id.as_deref(),
        ) {
            crate::cartographer::Note::new("followup")
                .level(crate::logging::LogLevel::WARNING)
                .scope("followup")
                .guardian(guardian_id)
                .emit(
                    self,
                    format!("review {guardian_id}: could not send the follow-up offer: {e}"),
                    serde_json::json!({"error": e.to_string()}),
                );
        }
        Ok(true)
    }

    /// Declines an open offer. Nothing is created.
    pub fn decline_followup_offer(&self, guardian_id: &str) -> Result<FollowupOfferView> {
        let changed = self.conn.execute(
            "UPDATE followup_offers SET status=?, resolved_at_ms=? WHERE guardian_id=? AND status=?",
            params![STATUS_DECLINED, now_ms(), guardian_id, STATUS_OFFERED],
        )?;
        self.answered_offer(guardian_id, changed, "declined")
    }

    /// The squad each accepted item of `guardian_id`'s offer created, by
    /// prophecy id. An empty id is an item claimed but not yet created.
    pub fn followup_item_squads(
        &self,
        guardian_id: &str,
    ) -> Result<std::collections::HashMap<i64, String>> {
        let mut stmt = self.conn.prepare(
            "SELECT prophecy_id, squad_id FROM followup_item_results WHERE guardian_id=?",
        )?;
        let rows = stmt
            .query_map(params![guardian_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Claims one item for acceptance. Returns `false` when it was already
    /// claimed, so two concurrent accepts cannot both create its squad. Pair
    /// with [`Self::release_followup_item`] if creating the squad fails.
    pub fn claim_followup_item(&self, guardian_id: &str, prophecy_id: i64) -> Result<bool> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO followup_item_results(guardian_id, prophecy_id, squad_id, created_at_ms)
             VALUES(?,?,'',?)",
            params![guardian_id, prophecy_id, now_ms()],
        )?;
        Ok(inserted > 0)
    }

    /// Gives back a claim whose squad could not be created, so the item can
    /// be accepted again.
    pub fn release_followup_item(&self, guardian_id: &str, prophecy_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM followup_item_results WHERE guardian_id=? AND prophecy_id=? AND squad_id=''",
            params![guardian_id, prophecy_id],
        )?;
        Ok(())
    }

    /// Records the squad a claimed item produced, and marks the
    /// whole offer `accepted` once every item in its snapshot has a squad.
    pub fn record_followup_item_result(
        &self,
        guardian_id: &str,
        prophecy_id: i64,
        squad_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE followup_item_results SET squad_id=? WHERE guardian_id=? AND prophecy_id=?",
            params![squad_id, guardian_id, prophecy_id],
        )?;
        let Some(offer) = self.get_followup_offer(guardian_id)? else {
            return Ok(());
        };
        let done = self.followup_item_squads(guardian_id)?;
        let all = offer
            .items
            .iter()
            .all(|i| done.get(&i.prophecy_id).is_some_and(|s| !s.is_empty()));
        if all {
            self.conn.execute(
                "UPDATE followup_offers SET status=?, squad_id=?, resolved_at_ms=?
                 WHERE guardian_id=? AND status=?",
                params![
                    STATUS_ACCEPTED,
                    squad_id,
                    now_ms(),
                    guardian_id,
                    STATUS_OFFERED
                ],
            )?;
        }
        Ok(())
    }

    fn answered_offer(
        &self,
        guardian_id: &str,
        changed: usize,
        verb: &str,
    ) -> Result<FollowupOfferView> {
        let Some(offer) = self.get_followup_offer(guardian_id)? else {
            return Err(StoreError::NotFound);
        };
        if changed == 0 {
            return Err(StoreError::InvalidTransition(format!(
                "review {guardian_id}'s follow-up offer is already {}, so it cannot be {verb}",
                offer.status
            )));
        }
        Ok(offer)
    }
}

/// The mailbox message announcing an offer.
#[must_use]
pub fn offer_message(guardian_id: &str, review_name: &str, items: &[FollowupItem]) -> String {
    let mut out = format!(
        "review {guardian_id} ({review_name}) merged with {} deferred follow-up suggestion(s):\n",
        items.len()
    );
    for item in items {
        out.push_str("- ");
        out.push_str(&truncate_chars(&item.body, MAX_MESSAGE_NOTE_CHARS));
        out.push('\n');
    }
    out.push_str(&format!(
        "Run `ralphus review followup accept {guardian_id}` to draft a follow-up squad from \
         them, or `ralphus review followup decline {guardian_id}` to drop the offer."
    ));
    out
}

/// The mailbox message saying an earlier [`offer_message`] was taken up, so a
/// reader who still has that message open knows it needs no action.
#[must_use]
pub fn accepted_message(
    guardian_id: &str,
    review_name: &str,
    items: &[FollowupItem],
    squad_id: &str,
) -> String {
    let mut out = format!(
        "The follow-up offer for review {guardian_id} ({review_name}) was accepted; squad \
         {squad_id} was created for:\n"
    );
    for item in items {
        out.push_str("- ");
        out.push_str(&truncate_chars(&item.body, MAX_MESSAGE_NOTE_CHARS));
        out.push('\n');
    }
    out.push_str("The earlier offer message needs no action for these items.");
    out
}

/// A name safe to use as a task id and as a branch component.
fn slug(text: &str) -> String {
    let slug: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug.trim_matches('-').to_string()
}

/// The prompt a follow-up task hands its agent.
fn task_prompt(review_name: &str, item: &FollowupItem) -> String {
    let mut out = format!(
        "This is follow-up work deferred while building review \"{review_name}\", which has \
         since merged.\n\nThe deferred item:\n{}\n",
        item.body
    );
    if let Some(prompt) = item.prompt.as_deref() {
        out.push_str(&format!(
            "\nFor context, this is the task the agent that deferred it was working on:\n{prompt}\n"
        ));
    }
    out.push_str(
        "\nDo the deferred work described above and nothing else. The original work is already \
         merged; build on the current state of the branch.\n\nWhen the work is done, commit \
         all of it on the current branch with a clear commit message, then push that branch \
         (`git push -u origin HEAD`). Work that is left uncommitted is not seen, and the task \
         fails for having no commits.",
    );
    out
}

/// Drafts the follow-up squad's task file: one task per deferred item, each
/// on its own worktree branched from `base`.
///
/// The file names no `[[review]]`. Every cell opts into Triage
/// (`triage = true`, no `triage_type`) so the Arbiter classifies it into a
/// registered type and pools it with other work of that `(project, type)`;
/// the pool's drain slots the follow-up work into an automatic review, the
/// same path ordinary Triage cells take.
#[must_use]
pub fn draft_followup_toml(
    project: &str,
    guardian: &GuardianView,
    base: &UpstreamRef,
    items: &[FollowupItem],
) -> String {
    let guardian_slug = slug(&guardian.id);
    // A follow-up runs where the work it follows up on ran.
    let machine = guardian.machine.as_deref().filter(|m| !m.is_empty());
    let tasks: Vec<toml::Value> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let n = i + 1;
            let branch = format!("followup-{guardian_slug}-{n}");
            let mut cell = toml::Table::new();
            cell.insert(
                "cwd".into(),
                format!(
                    "<<ralphus:new-worktree/{branch}?upstream_remote={}&upstream_branch={}>>",
                    base.remote, base.branch
                )
                .into(),
            );
            cell.insert("prompt".into(), task_prompt(&guardian.name, item).into());
            cell.insert("triage".into(), true.into());
            if let Some(agent) = item.agent.as_deref().filter(|a| !a.is_empty()) {
                cell.insert("agent".into(), agent.into());
            }
            if let Some(model) = item.model.as_deref().filter(|m| !m.is_empty()) {
                cell.insert("model".into(), model.into());
            }
            let mut task = toml::Table::new();
            task.insert("name".into(), format!("followup-{n}").into());
            task.insert("project".into(), project.into());
            if let Some(machine) = machine {
                task.insert("machine".into(), machine.into());
            }
            task.insert("cell".into(), toml::Value::Array(vec![cell.into()]));
            toml::Value::Table(task)
        })
        .collect();
    let mut root = toml::Table::new();
    root.insert("task".into(), toml::Value::Array(tasks));
    toml::to_string(&root).unwrap_or_default()
}

/// The proof steps one follow-up task inherits from the work it follows up
/// on, as `[[task.cell.proof]]` / `[[task.proof]]` tables ready to splice in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InheritedProofs {
    /// Cell-scope steps, attached to the follow-up task's one cell.
    pub cell: Vec<toml::Table>,
    /// Task-scope steps, attached to the follow-up task itself.
    pub task: Vec<toml::Table>,
    /// Where the steps came from, for the log line (`cell:...`, or the review
    /// whose linked cells stood in for an origin that no longer resolves).
    pub source: String,
}

/// One row of `proofs`, as far as a follow-up copies it.
struct StoredProof {
    id: Option<String>,
    kind: String,
    spec: String,
    agent: String,
    model: Option<String>,
    timeout_sec: Option<i64>,
    budget_tokens: Option<i64>,
    maximum_tool_output_tokens: Option<i64>,
    maximum_timeout_sec: Option<i64>,
    mode: Option<String>,
    remediation_attempts: Option<i64>,
    pass_score: Option<i64>,
}

/// The task-file table for one stored proof step, or `None` for a row that is
/// neither a `command` nor a `prompt` proof. Only the step's own definition is
/// copied: `restart_on` names steps of the original squad, `environment`
/// holds values resolved against the original worktree, and `extends`
/// presets are already stamped into the stored fields, so none of them mean
/// anything in the new squad. `agent` is written only when the step overrode
/// the agent of the cell it belongs to (`cell_agent`).
fn inherited_proof_table(proof: &StoredProof, cell_agent: &str) -> Option<toml::Table> {
    let mut step = toml::Table::new();
    match proof.kind.as_str() {
        "command" => {
            step.insert("command".into(), proof.spec.clone().into());
            if let Some(mode) = proof.mode.as_deref().filter(|m| !m.is_empty()) {
                step.insert("mode".into(), mode.into());
            }
            if let Some(attempts) = proof.remediation_attempts {
                step.insert("remediation_attempts".into(), attempts.into());
            }
        }
        "prompt" => {
            step.insert("prompt".into(), proof.spec.clone().into());
            if let Some(score) = proof.pass_score {
                step.insert("pass_score".into(), score.into());
            }
        }
        _ => return None,
    }
    if let Some(id) = proof.id.as_deref().filter(|i| !i.is_empty()) {
        step.insert("id".into(), id.into());
    }
    if !proof.agent.is_empty() && proof.agent != cell_agent {
        step.insert("agent".into(), proof.agent.clone().into());
    }
    if let Some(model) = proof.model.as_deref().filter(|m| !m.is_empty()) {
        step.insert("model".into(), model.into());
    }
    if let Some(secs) = proof.timeout_sec.filter(|s| *s > 0) {
        step.insert("timeout_minutes".into(), ((secs + 59) / 60).into());
    }
    if let Some(tokens) = proof.budget_tokens.filter(|t| *t > 0) {
        step.insert("budget_tokens".into(), tokens.into());
    }
    if let Some(tokens) = proof.maximum_tool_output_tokens.filter(|t| *t > 0) {
        step.insert("maximum_tool_output_tokens".into(), tokens.into());
    }
    if let Some(secs) = proof.maximum_timeout_sec.filter(|s| *s > 0) {
        step.insert("maximum_timeout_seconds".into(), secs.into());
    }
    Some(step)
}

/// Adds each `inherited[i]` to the `i`-th task of a drafted follow-up task
/// file: cell-scope steps under its first cell, task-scope steps on the task.
/// A task with no entry (or an empty one) is left as drafted, and so is the
/// whole file if it does not parse.
#[must_use]
pub fn attach_inherited_proofs(draft: &str, inherited: &[InheritedProofs]) -> String {
    if inherited
        .iter()
        .all(|p| p.cell.is_empty() && p.task.is_empty())
    {
        return draft.to_string();
    }
    let Ok(mut root) = draft.parse::<toml::Table>() else {
        return draft.to_string();
    };
    if let Some(toml::Value::Array(tasks)) = root.get_mut("task") {
        for (task, proofs) in tasks.iter_mut().zip(inherited) {
            let Some(task) = task.as_table_mut() else {
                continue;
            };
            if !proofs.cell.is_empty() {
                if let Some(toml::Value::Array(cells)) = task.get_mut("cell") {
                    if let Some(cell) = cells.first_mut().and_then(toml::Value::as_table_mut) {
                        cell.insert("proof".into(), table_array(&proofs.cell));
                    }
                }
            }
            if !proofs.task.is_empty() {
                task.insert("proof".into(), table_array(&proofs.task));
            }
        }
    }
    toml::to_string(&root).unwrap_or_else(|_| draft.to_string())
}

fn table_array(steps: &[toml::Table]) -> toml::Value {
    toml::Value::Array(steps.iter().cloned().map(toml::Value::Table).collect())
}

impl Store {
    /// The proof steps of one stored scope (`"cell"` with the cell's index, or
    /// `"task"` with `-1`), in order.
    fn stored_proofs(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
    ) -> Result<Vec<StoredProof>> {
        let mut stmt = self.conn.prepare(
            "SELECT vid, kind, spec, agent, model, timeout_sec, budget_tokens,
                    maximum_tool_output_tokens, maximum_timeout_sec, mode,
                    remediation_attempts, pass_score
             FROM proofs WHERE squad_id=? AND task_idx=? AND scope=? AND cell_idx=?
             ORDER BY idx",
        )?;
        let rows = stmt
            .query_map(params![squad_id, task_idx, scope, cell_idx], |r| {
                Ok(StoredProof {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    spec: r.get(2)?,
                    agent: r.get(3)?,
                    model: r.get(4)?,
                    timeout_sec: r.get(5)?,
                    budget_tokens: r.get(6)?,
                    maximum_tool_output_tokens: r.get(7)?,
                    maximum_timeout_sec: r.get(8)?,
                    mode: r.get(9)?,
                    remediation_attempts: r.get(10)?,
                    pass_score: r.get(11)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The proofs of one cell and of its task, appended to `out` (steps
    /// already present by `(kind, spec)` are skipped, so several linked cells
    /// that share a proof contribute it once).
    fn collect_cell_proofs(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        out: &mut InheritedProofs,
    ) -> Result<()> {
        let cell_agent: String = self
            .conn
            .query_row(
                "SELECT agent FROM cells WHERE squad_id=? AND task_idx=? AND idx=?",
                params![squad_id, task_idx, cell_idx],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default();
        for (scope, idx, target) in [
            ("cell", cell_idx, &mut out.cell),
            ("task", -1, &mut out.task),
        ] {
            for proof in self.stored_proofs(squad_id, task_idx, scope, idx)? {
                if let Some(step) = inherited_proof_table(&proof, &cell_agent) {
                    if !target.contains(&step) {
                        target.push(step);
                    }
                }
            }
        }
        Ok(())
    }

    /// The proofs a follow-up for `item` inherits: those of the cell that
    /// deferred it and of that cell's task. When that cell no longer exists
    /// (its squad was deleted, or a review-level deferral never had one), the
    /// proofs of the cells linked to `guardian_id`'s branches stand in, newest
    /// cell per branch. A store error yields no proofs rather than blocking
    /// the follow-up.
    #[must_use]
    pub fn followup_inherited_proofs(
        &self,
        guardian_id: &str,
        item: &FollowupItem,
    ) -> InheritedProofs {
        let mut out = InheritedProofs::default();
        let origin = match crate::entity_uri::parse(&item.entity_uri) {
            Some(crate::entity_uri::EntityUri::Cell {
                squad_id,
                task_idx,
                cell_idx,
            }) if self.cell_context_for_uri(&item.entity_uri).is_some() => {
                Some((squad_id, task_idx, cell_idx))
            }
            _ => None,
        };
        let result = if let Some((squad_id, task_idx, cell_idx)) = origin {
            out.source = item.entity_uri.clone();
            self.collect_cell_proofs(&squad_id, task_idx, cell_idx, &mut out)
        } else {
            out.source = format!("the cells linked to review {guardian_id}");
            self.collect_linked_cell_proofs(guardian_id, &mut out)
        };
        if let Err(e) = result {
            crate::rlog!(
                WARNING,
                "ralphus [followup] could not read the proofs of {}: {e}",
                out.source
            );
            crate::cartographer::Note::new("followup")
                .scope("followup")
                .guardian(guardian_id)
                .emit(
                    self,
                    format!(
                        "review {guardian_id}: could not read the proofs of {} for a follow-up",
                        out.source
                    ),
                    serde_json::json!({"source": out.source, "error": e.to_string()}),
                );
            return InheritedProofs::default();
        }
        crate::rlog!(
            INFO,
            "ralphus [followup] {} inherits {} cell proof(s) and {} task proof(s) from {}",
            item.entity_uri,
            out.cell.len(),
            out.task.len(),
            out.source
        );
        crate::cartographer::Note::new("followup")
            .scope("followup")
            .guardian(guardian_id)
            .emit(
                self,
                format!(
                    "review {guardian_id}: follow-up for {} inherits {} cell proof(s) and {} task \
                     proof(s) from {}",
                    item.entity_uri,
                    out.cell.len(),
                    out.task.len(),
                    out.source
                ),
                serde_json::json!({
                    "entity_uri": item.entity_uri,
                    "cell_proofs": out.cell.len(),
                    "task_proofs": out.task.len(),
                    "source": out.source,
                }),
            );
        out
    }

    /// Fallback for [`Self::followup_inherited_proofs`]: the proofs of the
    /// newest cell on each of the review's branches.
    fn collect_linked_cell_proofs(
        &self,
        guardian_id: &str,
        out: &mut InheritedProofs,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT squad_id, task_idx, idx, review_branch FROM cells
             WHERE review_guardian_id=? AND review_branch IS NOT NULL
             ORDER BY rowid DESC",
        )?;
        let linked = stmt
            .query_map(params![guardian_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut seen = std::collections::HashSet::new();
        for (squad_id, task_idx, cell_idx, branch) in linked {
            if seen.insert(branch) {
                self.collect_cell_proofs(&squad_id, task_idx, cell_idx, out)?;
            }
        }
        Ok(())
    }
}

/// Whether `base` exists on its remote, falling back to the local refs when
/// the remote cannot be asked (no such remote, offline). A local base is
/// looked up in the local refs only.
fn branch_exists(root: &Path, base: &UpstreamRef) -> bool {
    let git = |args: &[&str]| {
        ralphus_core::git_spawn::command(args)
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
    };
    let heads = format!("refs/heads/{}", base.branch);
    if !base.is_local() {
        if let Ok(out) = git(&["ls-remote", "--exit-code", "--heads", &base.remote, &heads]) {
            match out.status.code() {
                Some(0) => return true,
                // `--exit-code`: 2 means the remote answered and has no such ref.
                Some(2) => return false,
                _ => {}
            }
        }
    }
    git(&["rev-parse", "--verify", "--quiet", &base.tracking_ref()])
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The upstream a follow-up squad is based on: exactly the review's own
/// (remote, branch) pair, or an error naming the pair when it no longer exists.
pub fn resolve_followup_base(
    git_root: &Path,
    merged_into: &UpstreamRef,
) -> std::result::Result<UpstreamRef, String> {
    if !merged_into.branch.is_empty() && branch_exists(git_root, merged_into) {
        Ok(merged_into.clone())
    } else {
        Err(format!(
            "the review's upstream remote '{}' / branch '{}' no longer exists, so a follow-up              squad has nothing to be based on: restore the branch or point the review at another              upstream, then accept again",
            merged_into.remote, merged_into.branch
        ))
    }
}

/// What accepting an offer created.
#[derive(Debug, Clone, Serialize)]
pub struct AcceptResult {
    pub squad_id: String,
    /// The upstream the squad is based on (`<<default>>` when the branch the
    /// review merged into no longer exists).
    pub base: String,
    /// Whether the squad was submitted to run at once (`auto_start`) rather
    /// than held for the user to start.
    pub started: bool,
}

/// Why accepting an offer failed.
#[derive(Debug)]
pub enum AcceptError {
    Store(StoreError),
    /// The follow-up could not be created; carries the HTTP status and a
    /// message that says what to do about it.
    Rejected {
        status: u16,
        message: String,
    },
}

impl From<StoreError> for AcceptError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// Accepts an open offer: drafts the follow-up squad, submits it through the
/// ordinary submit path (so every preflight applies).
///
/// The offer is claimed first, atomically, so a second accept -- a retried
/// request, a double click -- finds it already accepted and creates nothing.
/// A failure before the squad exists puts the claim back so the user can try
/// again; a failure after it leaves the offer accepted, since a second squad
/// would duplicate the first.
///
/// `only` accepts a single item of the offer (by prophecy id); `None` accepts
/// every item not yet accepted. Each accept makes its own squad, and the offer
/// is `accepted` once all its items have one.
pub(crate) fn accept_offer(
    daemon: &crate::server::Daemon,
    guardian_id: &str,
    only: Option<i64>,
) -> std::result::Result<AcceptResult, AcceptError> {
    let (offer, guardian) = {
        let store = daemon.lock();
        let guardian = store.get_guardian(guardian_id)?;
        let mut offer = store
            .get_followup_offer(guardian_id)?
            .ok_or(StoreError::NotFound)?;
        if offer.status != STATUS_OFFERED {
            return Err(StoreError::InvalidTransition(format!(
                "review {guardian_id}'s follow-up offer is already {}, so it cannot be accepted",
                offer.status
            ))
            .into());
        }
        if let Some(id) = only {
            if !offer.items.iter().any(|i| i.prophecy_id == id) {
                return Err(StoreError::NotFound.into());
            }
        }
        let taken = store.followup_item_squads(guardian_id)?;
        offer.items.retain(|i| {
            only.is_none_or(|id| i.prophecy_id == id) && !taken.contains_key(&i.prophecy_id)
        });
        if offer.items.is_empty() {
            return Err(StoreError::InvalidTransition(format!(
                "review {guardian_id}'s follow-up item is already accepted"
            ))
            .into());
        }
        let mut claimed = Vec::new();
        for item in &offer.items {
            if store.claim_followup_item(guardian_id, item.prophecy_id)? {
                claimed.push(item.prophecy_id);
            } else {
                for id in claimed {
                    let _ = store.release_followup_item(guardian_id, id);
                }
                return Err(StoreError::InvalidTransition(format!(
                    "review {guardian_id}'s follow-up item is already being accepted"
                ))
                .into());
            }
        }
        (offer, guardian)
    };
    let (squad_id, base, started) = match submit_followup_squad(daemon, &offer, &guardian) {
        Ok(created) => created,
        Err(e) => {
            let store = daemon.lock();
            for item in &offer.items {
                let _ = store.release_followup_item(guardian_id, item.prophecy_id);
            }
            return Err(e);
        }
    };
    let store = daemon.lock();
    for item in &offer.items {
        store.record_followup_item_result(guardian_id, item.prophecy_id, &squad_id)?;
    }
    // The offer message sent at merge is now out of date; say so where the
    // same watchers will read it.
    if let Err(e) = store.notify_watchers(
        crate::monitor::NotifiableEventKind::ReviewFollowupOffered,
        &format!("guardian:{guardian_id}"),
        crate::mailbox::MailboxPriority::Normal,
        &accepted_message(guardian_id, &guardian.name, &offer.items, &squad_id),
        guardian.squad_id.as_deref(),
    ) {
        crate::cartographer::Note::new("followup")
            .level(crate::logging::LogLevel::WARNING)
            .scope("followup")
            .guardian(guardian_id)
            .emit(
                &store,
                format!("review {guardian_id}: could not send the offer-accepted notice: {e}"),
                serde_json::json!({"error": e.to_string()}),
            );
    }
    Ok(AcceptResult {
        squad_id,
        base: base.joined(),
        started,
    })
}

/// Name of the supervised thread that submits items accepted before the merge.
pub const PREACCEPTED_THREAD: &str = "followup-preaccepted";

/// How often that thread looks for newly made offers.
const PREACCEPTED_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// How long an item whose submission failed is left alone before it is tried
/// again; a rejection does not fix itself in seconds, and each try would send
/// the failure notice again.
const PREACCEPTED_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(600);

/// Submits every item accepted ahead of the merge whose offer now exists,
/// skipping items in `failed` that failed less than [`PREACCEPTED_RETRY_AFTER`]
/// ago. Returns how many squads it created.
pub(crate) fn submit_preaccepted_once(
    daemon: &crate::server::Daemon,
    failed: &mut std::collections::HashMap<(String, i64), std::time::Instant>,
) -> usize {
    let due = match daemon.lock().due_preaccepted_followups() {
        Ok(due) => due,
        Err(_) => return 0,
    };
    let mut created = 0;
    for (guardian_id, prophecy_id) in due {
        let key = (guardian_id.clone(), prophecy_id);
        if failed
            .get(&key)
            .is_some_and(|at| at.elapsed() < PREACCEPTED_RETRY_AFTER)
        {
            continue;
        }
        match accept_offer(daemon, &guardian_id, Some(prophecy_id)) {
            Ok(_) => {
                failed.remove(&key);
                created += 1;
            }
            // Answered between the listing and the claim.
            Err(AcceptError::Store(StoreError::InvalidTransition(_))) => {}
            Err(e) => {
                failed.insert(key, std::time::Instant::now());
                let detail = match e {
                    AcceptError::Rejected { message, .. } => message,
                    AcceptError::Store(e) => e.to_string(),
                };
                let store = daemon.lock();
                let _ = store.enqueue_error_mailbox_message_deduped(
                    crate::mailbox::MailboxPriority::Normal,
                    &format!(
                        "review {guardian_id}: a follow-up you accepted before the merge could \
                         not be submitted: {detail}"
                    ),
                    &crate::mailbox::Remediation::SuggestedCommand {
                        command: format!("ralphus review followup accept {guardian_id}"),
                        purpose: "submit the follow-up once the cause is fixed".to_string(),
                    },
                    None,
                    None,
                    None,
                    Some(&format!("guardian:{guardian_id}")),
                    Some("followup"),
                );
            }
        }
    }
    created
}

/// Starts the thread that submits items accepted before their review merged,
/// so they become squads as soon as the merge makes the offer.
pub fn spawn_preaccepted_worker(daemon: std::sync::Arc<crate::server::Daemon>) {
    crate::thread_health::spawn_supervised(
        crate::thread_health::ThreadSpec {
            name: PREACCEPTED_THREAD,
            description: "Follow-ups accepted before a review merged are not submitted when it \
                          merges.",
            max_silence: None,
        },
        daemon.store_handle(),
        std::sync::Arc::new(move || {
            let mut failed = std::collections::HashMap::new();
            loop {
                submit_preaccepted_once(&daemon, &mut failed);
                std::thread::sleep(PREACCEPTED_INTERVAL);
            }
        }),
    );
}

/// Drafts and submits the squad; returns `(squad_id, base, started)`.
fn submit_followup_squad(
    daemon: &crate::server::Daemon,
    offer: &FollowupOfferView,
    guardian: &GuardianView,
) -> std::result::Result<(String, UpstreamRef, bool), AcceptError> {
    let rejected = |status: u16, message: String| AcceptError::Rejected { status, message };
    let Some(project) = guardian.project.clone() else {
        return Err(rejected(
            400,
            format!(
                "review {} has no registered project, so a follow-up squad has nowhere to run: \
                 register the repository with `ralphus project git` and accept again",
                guardian.id
            ),
        ));
    };
    if offer.items.is_empty() {
        return Err(rejected(
            400,
            format!("review {}'s offer carries no deferred items", guardian.id),
        ));
    }
    let root = Path::new(&guardian.git_root);
    let base = resolve_followup_base(root, &guardian.upstream()).map_err(|m| rejected(409, m))?;
    let started = guardian.effective_followup_auto_start;
    let inherited: Vec<InheritedProofs> = {
        let store = daemon.lock();
        offer
            .items
            .iter()
            .map(|item| store.followup_inherited_proofs(&guardian.id, item))
            .collect()
    };
    let draft = attach_inherited_proofs(
        &draft_followup_toml(&project, guardian, &base, &offer.items),
        &inherited,
    );
    let body = serde_json::json!({
        "toml": draft,
        "hold": !started,
        "label": format!("follow-up: {}", guardian.name),
    })
    .to_string();
    let reply = crate::server::submit(daemon, &body, "", None);
    if reply.status >= 300 {
        return Err(rejected(
            reply.status,
            format!("the follow-up squad was rejected: {}", reply.body),
        ));
    }
    let squad_id = serde_json::from_str::<serde_json::Value>(&reply.body)
        .ok()
        .and_then(|v| v["squad_id"].as_str().map(str::to_string))
        .ok_or_else(|| rejected(500, "the submit reply named no squad".to_string()))?;
    Ok((squad_id, base, started))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FollowupConfig;
    use std::process::Command;

    fn item(body: &str, prompt: Option<&str>) -> FollowupItem {
        FollowupItem {
            prophecy_id: 1,
            entity_uri: "cell:squad-1:0:0".to_string(),
            body: body.to_string(),
            prompt: prompt.map(str::to_string),
            agent: Some("claude-code".to_string()),
            model: Some("sonnet".to_string()),
        }
    }

    #[test]
    fn truncate_keeps_short_text_and_marks_cut_text() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("abcdefghij", 4), "abcd…");
    }

    #[test]
    fn slug_is_lowercase_alphanumeric_with_dashes() {
        assert_eq!(slug("Guardian_0000/12"), "guardian-0000-12");
        assert_eq!(slug("--x--"), "x");
    }

    #[test]
    fn offer_message_names_the_commands_that_answer_it() {
        let msg = offer_message("guardian-1", "my review", &[item("add the cache", None)]);
        assert!(msg.contains("guardian-1"));
        assert!(msg.contains("add the cache"));
        assert!(msg.contains("ralphus review followup accept guardian-1"));
        assert!(msg.contains("ralphus review followup decline guardian-1"));
    }

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    /// A review whose git root is a throwaway directory.
    fn seed_guardian(s: &Store, id: &str, root: &Path) {
        s.conn
            .execute(
                "INSERT INTO guardians(id, name, base_branch, git_root, status, created_at_ms, updated_at_ms, project)
                 VALUES (?,'the review','main',?,'in_review',0,0,'proj')",
                params![id, root.to_string_lossy()],
            )
            .unwrap();
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-followup-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn defer(s: &Store, guardian_id: &str, kind: crate::prophecy::ProphecyKind, body: &str) {
        s.add_prophecy(
            "guardian:ignored",
            0,
            kind,
            body,
            None,
            None,
            Some(guardian_id),
        )
        .unwrap();
    }

    fn merge(s: &Store, id: &str) {
        s.set_guardian_status(id, crate::guardian::GuardianStatus::Merged, None)
            .unwrap();
    }

    #[test]
    fn merging_a_review_with_deferred_prophecies_offers_once() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("once");
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "add the cache");
        defer(&s, "g1", ProphecyKind::Deferred, "document the flag");
        defer(&s, "g1", ProphecyKind::Hazard, "not a follow-up");

        merge(&s, "g1");
        let offer = s
            .get_followup_offer("g1")
            .unwrap()
            .expect("offered at merge");
        assert_eq!(offer.status, STATUS_OFFERED);
        assert_eq!(
            offer.items.len(),
            2,
            "only the deferred prophecies are offered"
        );
        assert_eq!(offer.depth, 0);

        // Reopening and re-merging must not offer a second time, even with a
        // new deferred prophecy in between.
        s.reopen_guardian("g1").unwrap();
        defer(&s, "g1", ProphecyKind::Deferred, "a later note");
        merge(&s, "g1");
        assert_eq!(s.get_followup_offer("g1").unwrap().unwrap().items.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_review_without_deferred_prophecies_offers_nothing() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("none");
        seed_guardian(&s, "g1", &root);
        defer(
            &s,
            "g1",
            ProphecyKind::Unconfirmed,
            "could not run the suite",
        );
        defer(&s, "g1", ProphecyKind::Decision, "kept it simple");
        merge(&s, "g1");
        assert!(s.get_followup_offer("g1").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_a_merged_review_offers() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("approved");
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "later");
        s.set_guardian_status("g1", crate::guardian::GuardianStatus::Approved, None)
            .unwrap();
        assert!(s.get_followup_offer("g1").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_review_can_turn_offers_off() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("off");
        seed_guardian(&s, "g1", &root);
        s.set_guardian_followup_enabled("g1", Some(false)).unwrap();
        defer(&s, "g1", ProphecyKind::Deferred, "later");
        merge(&s, "g1");
        assert!(s.get_followup_offer("g1").unwrap().is_none());

        // Clearing the override inherits the global default, which is on.
        s.set_guardian_followup_enabled("g1", None).unwrap();
        assert!(
            s.maybe_offer_followups_with("g1", &FollowupConfig::default())
                .unwrap()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_review_can_turn_offers_on_when_the_global_config_has_them_off() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("on");
        seed_guardian(&s, "g1", &root);
        s.set_guardian_followup_enabled("g1", Some(true)).unwrap();
        defer(&s, "g1", ProphecyKind::Deferred, "later");
        let off = FollowupConfig {
            enabled: Some(false),
            ..FollowupConfig::default()
        };
        assert!(s.maybe_offer_followups_with("g1", &off).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn follow_up_settings_default_on_and_resolve_per_review() {
        let s = store();
        let root = temp_root("settings");
        seed_guardian(&s, "g1", &root);
        let g = s.get_guardian("g1").unwrap();
        assert_eq!(g.followup_enabled, None);
        assert_eq!(g.followup_auto_start, None);
        assert!(g.effective_followup_enabled, "offers default on");
        assert!(g.effective_followup_auto_start, "auto-start defaults on");

        s.set_guardian_followup_auto_start("g1", Some(false))
            .unwrap();
        let g = s.get_guardian("g1").unwrap();
        assert_eq!(g.followup_auto_start, Some(false));
        assert!(!g.effective_followup_auto_start);
        assert!(s.set_guardian_followup_enabled("nope", Some(true)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_follow_up_squads_review_is_at_the_depth_cap() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("depth");
        seed_guardian(&s, "g1", &root);
        seed_guardian(&s, "g2", &root);
        // g2 is the review of the squad that accepting g1's offer created.
        s.conn
            .execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES ('squad-f','pending',0,0)",
                [],
            )
            .unwrap();
        s.conn
            .execute("UPDATE guardians SET squad_id='squad-f' WHERE id='g2'", [])
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO followup_offers(guardian_id, status, items, depth, squad_id, created_at_ms)
                 VALUES ('g1','accepted','[]',0,'squad-f',0)",
                [],
            )
            .unwrap();
        assert_eq!(s.followup_depth_of_review("g1").unwrap(), 0);
        assert_eq!(s.followup_depth_of_review("g2").unwrap(), 1);

        defer(
            &s,
            "g2",
            ProphecyKind::Deferred,
            "a follow-up's own deferral",
        );
        merge(&s, "g2");
        assert!(
            s.get_followup_offer("g2").unwrap().is_none(),
            "the default cap of 1 offers nothing for a follow-up's own deferrals"
        );

        // Raising the global cap lets the second generation offer.
        let deeper = FollowupConfig {
            max_depth: Some(2),
            ..FollowupConfig::default()
        };
        s.maybe_offer_followups_with("g2", &deeper).unwrap();
        let offer = s
            .get_followup_offer("g2")
            .unwrap()
            .expect("allowed at depth 2");
        assert_eq!(offer.depth, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_invalid_followup_config_offers_nothing() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("invalid-config");
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "later");
        let zero = FollowupConfig {
            max_depth: Some(0),
            ..FollowupConfig::default()
        };
        assert!(
            !s.maybe_offer_followups_with("g1", &zero).unwrap(),
            "max_depth = 0 while enabled is rejected, so nothing is offered"
        );
        assert!(s.get_followup_offer("g1").unwrap().is_none());

        // Nothing was recorded, so a fixed config can still offer.
        assert!(
            s.maybe_offer_followups_with("g1", &FollowupConfig::default())
                .unwrap()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn defer_as(s: &Store, guardian_id: &str, owner: &str, attempt: i64, body: &str) -> i64 {
        s.add_prophecy(
            owner,
            attempt,
            crate::prophecy::ProphecyKind::Deferred,
            body,
            None,
            None,
            Some(guardian_id),
        )
        .unwrap()
        .id
    }

    #[test]
    fn only_the_latest_attempt_of_each_owner_is_pending() {
        let s = store();
        let root = temp_root("attempts");
        seed_guardian(&s, "g1", &root);
        defer_as(&s, "g1", "guardian:a", 0, "old a");
        let new_a = defer_as(&s, "g1", "guardian:a", 1, "new a");
        let b = defer_as(&s, "g1", "guardian:b", 0, "only b");

        let items = s.pending_followup_items("g1").unwrap();
        let mut ids: Vec<i64> = items.iter().map(|i| i.item.prophecy_id).collect();
        ids.sort();
        assert_eq!(ids, vec![new_a, b]);

        merge(&s, "g1");
        let offer = s.get_followup_offer("g1").unwrap().expect("offered");
        let mut offered: Vec<&str> = offer.items.iter().map(|i| i.body.as_str()).collect();
        offered.sort();
        assert_eq!(
            offered,
            vec!["new a", "only b"],
            "snapshot uses the same rule"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pending_item_names_the_branch_and_task_of_its_cell() {
        let mut s = store();
        let root = temp_root("branch");
        seed_guardian(&s, "g1", &root);
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            r#"
[[task]]
name = "Add feature"
[[task.cell]]
cwd = "/repo"
prompt = "add feature.txt"
"#,
        )
        .expect("valid toml");
        let squad_id = s.insert_squad(&file, None, false).unwrap();
        s.set_cell_review_branch(&squad_id, 0, 0, "feat/readme")
            .unwrap();
        let owner = format!("cell:{squad_id}:0:0");
        defer_as(&s, "g1", &owner, 0, "document it");
        defer_as(&s, "g1", "guardian:gone", 0, "orphan");

        let items = s.pending_followup_items("g1").unwrap();
        let with_cell = items
            .iter()
            .find(|i| i.item.body == "document it")
            .expect("the cell-owned deferral");
        assert_eq!(with_cell.branch.as_deref(), Some("feat/readme"));
        assert_eq!(with_cell.task_name.as_deref(), Some("Add feature"));
        let orphan = items
            .iter()
            .find(|i| i.item.body == "orphan")
            .expect("the guardian-owned deferral");
        assert_eq!(
            (orphan.branch.as_deref(), orphan.task_name.as_deref()),
            (None, None)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    const PROOF_SQUAD: &str = r#"
[[task]]
name = "Add feature"
timeout_minutes = 10
[[task.cell]]
cwd = "/repo"
prompt = "add feature.txt"
agent = "claude-code"
[[task.cell.proof]]
id = "unit"
command = "cargo test"
mode = "remediating"
remediation_attempts = 3
[[task.cell.proof]]
prompt = "check the feature reads well"
pass_score = 7
agent = "codex"
[[task.proof]]
command = "cargo clippy"
mode = "raw"
"#;

    fn seed_proof_squad(s: &mut Store, guardian_id: &str) -> String {
        let file: ralphus_core::schema::TaskFile = toml::from_str(PROOF_SQUAD).expect("valid toml");
        let squad_id = s.insert_squad(&file, None, false).unwrap();
        s.set_cell_review_branch(&squad_id, 0, 0, "feat/readme")
            .unwrap();
        s.conn
            .execute(
                "UPDATE cells SET review_guardian_id=? WHERE squad_id=?",
                params![guardian_id, squad_id],
            )
            .unwrap();
        squad_id
    }

    fn item_for(owner: &str) -> FollowupItem {
        FollowupItem {
            entity_uri: owner.to_string(),
            ..item("document it", None)
        }
    }

    #[test]
    fn a_follow_up_inherits_the_proofs_of_the_cell_that_deferred_it() {
        let mut s = store();
        let root = temp_root("inherit");
        seed_guardian(&s, "g1", &root);
        let squad_id = seed_proof_squad(&mut s, "g1");
        let got = s.followup_inherited_proofs("g1", &item_for(&format!("cell:{squad_id}:0:0")));
        assert_eq!(got.source, format!("cell:{squad_id}:0:0"));
        assert_eq!(got.cell.len(), 2);
        assert_eq!(got.cell[0]["command"].as_str(), Some("cargo test"));
        assert_eq!(got.cell[0]["id"].as_str(), Some("unit"));
        assert_eq!(got.cell[0]["mode"].as_str(), Some("remediating"));
        assert_eq!(got.cell[0]["remediation_attempts"].as_integer(), Some(3));
        assert_eq!(got.cell[0]["timeout_minutes"].as_integer(), Some(10));
        assert!(!got.cell[0].contains_key("agent"), "same agent as the cell");
        assert_eq!(got.cell[1]["pass_score"].as_integer(), Some(7));
        assert_eq!(got.cell[1]["agent"].as_str(), Some("codex"));
        assert_eq!(got.task.len(), 1);
        assert_eq!(got.task[0]["mode"].as_str(), Some("raw"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_follow_up_whose_origin_is_gone_uses_the_reviews_linked_cells() {
        let mut s = store();
        let root = temp_root("inherit-fallback");
        seed_guardian(&s, "g1", &root);
        seed_proof_squad(&mut s, "g1");
        for owner in ["cell:squad-deleted:0:0", "guardian:g1"] {
            let got = s.followup_inherited_proofs("g1", &item_for(owner));
            assert_eq!((got.cell.len(), got.task.len()), (2, 1), "{owner}");
            assert!(got.source.contains("g1"), "{}", got.source);
        }
        let none = s.followup_inherited_proofs("other", &item_for("guardian:other"));
        assert_eq!((none.cell.len(), none.task.len()), (0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn attached_proofs_land_on_the_right_task_and_validate() {
        let mut s = store();
        let root = temp_root("inherit-attach");
        seed_guardian(&s, "g1", &root);
        let squad_id = seed_proof_squad(&mut s, "g1");
        let guardian = s.get_guardian("g1").unwrap();
        let items = [
            item_for(&format!("cell:{squad_id}:0:0")),
            item_for("guardian:g1"),
        ];
        let inherited = [
            s.followup_inherited_proofs("g1", &items[0]),
            InheritedProofs::default(),
        ];
        let draft = draft_followup_toml(
            "proj",
            &guardian,
            &UpstreamRef::new("origin", "main"),
            &items,
        );
        let toml = attach_inherited_proofs(&draft, &inherited);
        let report = ralphus_core::validate::validate_toml(&toml);
        assert!(
            report.errors.is_empty(),
            "{:?}
{toml}",
            report.errors
        );
        let file: ralphus_core::schema::TaskFile = toml::from_str(&toml).unwrap();
        assert_eq!(file.task[0].cell[0].proof.len(), 2);
        assert_eq!(file.task[0].proof.len(), 1);
        assert!(file.task[1].cell[0].proof.is_empty() && file.task[1].proof.is_empty());
        assert_eq!(
            attach_inherited_proofs(&draft, &[InheritedProofs::default()]),
            draft,
            "nothing to attach leaves the draft untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignores_persist_and_are_left_out_of_the_offer() {
        let s = store();
        let root = temp_root("ignore");
        seed_guardian(&s, "g1", &root);
        let keep = defer_as(&s, "g1", "guardian:a", 0, "keep");
        let drop = defer_as(&s, "g1", "guardian:b", 0, "drop");

        assert_eq!(s.set_followup_ignored("g1", Some(drop), true).unwrap(), 1);
        assert_eq!(
            s.set_followup_ignored("g1", Some(drop), true).unwrap(),
            0,
            "ignoring twice changes nothing"
        );
        let summary = s.pending_followups_summary("g1").unwrap();
        assert_eq!((summary.count, summary.total), (1, 2));
        assert!(summary.enabled && !summary.off && !summary.offered);
        assert!(matches!(
            s.set_followup_ignored("g1", Some(99_999), true),
            Err(StoreError::NotFound)
        ));

        s.set_followup_ignored("g1", Some(drop), false).unwrap();
        assert_eq!(s.pending_followups_summary("g1").unwrap().count, 2);
        s.set_followup_ignored("g1", Some(drop), true).unwrap();

        merge(&s, "g1");
        let offer = s.get_followup_offer("g1").unwrap().expect("offered");
        let ids: Vec<i64> = offer.items.iter().map(|i| i.prophecy_id).collect();
        assert_eq!(ids, vec![keep]);
        assert_eq!(
            s.list_all_prophecies_for_guardian("g1").unwrap().len(),
            2,
            "the prophecy rows are untouched"
        );

        // The snapshot is final: nothing may change once the offer exists.
        assert!(matches!(
            s.set_followup_ignored("g1", Some(drop), false),
            Err(StoreError::InvalidTransition(_))
        ));
        let summary = s.pending_followups_summary("g1").unwrap();
        assert!(summary.offered && !summary.enabled);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignoring_every_deferral_offers_nothing_and_writes_no_row() {
        let s = store();
        let root = temp_root("ignore-all");
        seed_guardian(&s, "g1", &root);
        defer_as(&s, "g1", "guardian:a", 0, "one");
        defer_as(&s, "g1", "guardian:b", 0, "two");
        assert_eq!(s.set_followup_ignored("g1", None, true).unwrap(), 2);
        assert_eq!(s.pending_followups_summary("g1").unwrap().count, 0);

        merge(&s, "g1");
        assert!(s.get_followup_offer("g1").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_summary_reports_off_when_followups_are_disabled_for_the_review() {
        let s = store();
        let root = temp_root("summary-off");
        seed_guardian(&s, "g1", &root);
        assert_eq!(s.pending_followups_summary("g1").unwrap().total, 0);
        defer_as(&s, "g1", "guardian:a", 0, "one");
        s.set_guardian_followup_enabled("g1", Some(false)).unwrap();
        let summary = s.pending_followups_summary("g1").unwrap();
        assert!(summary.off && !summary.enabled);
        assert_eq!(summary.count, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_item_can_be_claimed_only_once_and_released_on_failure() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("claim");
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "first");
        defer(&s, "g1", ProphecyKind::Deferred, "second");
        merge(&s, "g1");
        let offer = s.get_followup_offer("g1").unwrap().unwrap();
        let (a, b) = (offer.items[0].prophecy_id, offer.items[1].prophecy_id);

        assert!(s.claim_followup_item("g1", a).unwrap());
        assert!(
            !s.claim_followup_item("g1", a).unwrap(),
            "a second claim must not create a second squad"
        );
        s.release_followup_item("g1", a).unwrap();
        assert!(s.claim_followup_item("g1", a).unwrap());

        // One item answered leaves the offer open; the last one closes it.
        s.record_followup_item_result("g1", a, "squad-a").unwrap();
        assert_eq!(
            s.get_followup_offer("g1").unwrap().unwrap().status,
            STATUS_OFFERED
        );
        let items = s.pending_followup_items("g1").unwrap();
        let squad_of = |id: i64| {
            items
                .iter()
                .find(|i| i.item.prophecy_id == id)
                .unwrap()
                .squad_id
                .clone()
        };
        assert_eq!(squad_of(a).as_deref(), Some("squad-a"));
        assert_eq!(squad_of(b), None);

        assert!(s.claim_followup_item("g1", b).unwrap());
        s.record_followup_item_result("g1", b, "squad-b").unwrap();
        assert_eq!(
            s.get_followup_offer("g1").unwrap().unwrap().status,
            STATUS_ACCEPTED
        );
        assert!(matches!(
            s.decline_followup_offer("g1"),
            Err(StoreError::InvalidTransition(_))
        ));
        assert!(s.list_open_waypoint_ids().unwrap().is_empty());
        assert!(s.list_closed_waypoint_ids().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_item_accepted_before_the_merge_is_due_once_the_offer_exists() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("preaccept");
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "first");
        defer(&s, "g1", ProphecyKind::Deferred, "second");
        let items = s.pending_followup_items("g1").unwrap();
        let (a, b) = (items[0].item.prophecy_id, items[1].item.prophecy_id);

        s.set_followup_ignored("g1", Some(a), true).unwrap();
        s.preaccept_followup("g1", a).unwrap();
        let items = s.pending_followup_items("g1").unwrap();
        assert!(
            items[0].accepted && !items[0].ignored,
            "accepting an ignored item un-ignores it"
        );
        assert!(!items[1].accepted);
        assert!(
            s.due_preaccepted_followups().unwrap().is_empty(),
            "nothing is due before the offer exists"
        );
        assert!(matches!(
            s.preaccept_followup("g1", 9999),
            Err(StoreError::NotFound)
        ));

        // Ignoring it again withdraws the acceptance.
        s.set_followup_ignored("g1", Some(a), true).unwrap();
        assert!(!s.pending_followup_items("g1").unwrap()[0].accepted);
        s.preaccept_followup("g1", a).unwrap();

        merge(&s, "g1");
        assert_eq!(
            s.due_preaccepted_followups().unwrap(),
            vec![("g1".to_string(), a)],
            "only the accepted item is due, not its sibling {b}"
        );
        assert!(matches!(
            s.preaccept_followup("g1", b),
            Err(StoreError::InvalidTransition(_))
        ));

        s.claim_followup_item("g1", a).unwrap();
        assert!(s.due_preaccepted_followups().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_offer_accepted_whole_reads_as_every_item_accepted() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("legacy");
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "first");
        merge(&s, "g1");
        // The state the whole-offer accept left behind: no per-item rows.
        s.conn
            .execute(
                "UPDATE followup_offers SET status='accepted', squad_id='squad-old' WHERE guardian_id='g1'",
                [],
            )
            .unwrap();
        let items = s.pending_followup_items("g1").unwrap();
        assert!(items[0].accepted);
        assert_eq!(items[0].squad_id.as_deref(), Some("squad-old"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn accepted_message_names_the_squad_and_the_items() {
        let msg = accepted_message(
            "guardian-1",
            "my review",
            &[item("add the cache", None)],
            "squad-9",
        );
        assert!(msg.contains("squad-9") && msg.contains("add the cache"));
        assert!(msg.contains("accepted"));
    }

    #[test]
    fn the_drafted_squad_is_a_valid_task_file_based_on_the_merge_target() {
        let s = store();
        let root = temp_root("draft");
        seed_guardian(&s, "g1", &root);
        let guardian = s.get_guardian("g1").unwrap();
        let toml = draft_followup_toml(
            "proj",
            &guardian,
            &UpstreamRef::new("origin", "release/1.0"),
            &[
                item("add the cache", Some("the original ask")),
                item("document \"the\" flag", None),
            ],
        );
        let report = ralphus_core::validate::validate_toml(&toml);
        assert!(report.errors.is_empty(), "{:?}\n{toml}", report.errors);
        let file: ralphus_core::schema::TaskFile = toml::from_str(&toml).unwrap();
        assert_eq!(file.task.len(), 2, "one task per deferred item");
        assert!(file.review.is_empty(), "a follow-up declares no review");
        assert!(!toml.contains("new-review"), "{toml}");
        let cell = &file.task[0].cell[0];
        assert_eq!(cell.review, None, "no explicit review on the cell");
        assert!(cell.triage, "the cell is routed by the Arbiter's Triage");
        assert_eq!(cell.triage_type, None, "the Arbiter picks the type");
        assert!(
            cell.cwd
                .as_deref()
                .unwrap()
                .contains("upstream_branch=release/1.0")
        );
        assert_eq!(
            cell.agent,
            Some(ralphus_core::schema::AgentSpec::Single(
                "claude-code".to_string()
            ))
        );
        assert!(cell.prompt.as_deref().unwrap().contains("add the cache"));
        assert!(cell.prompt.as_deref().unwrap().contains("the original ask"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_follow_up_runs_on_the_machine_its_review_ran_on() {
        let s = store();
        let root = temp_root("machine");
        seed_guardian(&s, "g1", &root);
        s.conn
            .execute(
                "UPDATE guardians SET machine='loopback:exercise' WHERE id='g1'",
                [],
            )
            .unwrap();
        let guardian = s.get_guardian("g1").unwrap();
        let toml = draft_followup_toml(
            "proj",
            &guardian,
            &UpstreamRef::new("origin", "main"),
            &[item("later", None)],
        );
        let file: ralphus_core::schema::TaskFile = toml::from_str(&toml).unwrap();
        assert_eq!(file.task[0].machine.as_deref(), Some("loopback:exercise"));

        let local = draft_followup_toml(
            "proj",
            &{
                s.conn
                    .execute("UPDATE guardians SET machine=NULL WHERE id='g1'", [])
                    .unwrap();
                s.get_guardian("g1").unwrap()
            },
            &UpstreamRef::new("origin", "main"),
            &[item("later", None)],
        );
        let file: ralphus_core::schema::TaskFile = toml::from_str(&local).unwrap();
        assert_eq!(file.task[0].machine, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_base_branch_is_an_error_naming_the_pair() {
        let root = temp_root("base");
        // Not a repository at all: nothing to find the branch in.
        let err = resolve_followup_base(&root, &UpstreamRef::local("gone")).unwrap_err();
        assert!(err.contains("'.'") && err.contains("'gone'"), "{err}");
        assert!(resolve_followup_base(&root, &UpstreamRef::local("")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn run_git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn the_base_is_the_merge_target_while_it_exists_and_an_error_once_it_is_gone() {
        let root = temp_root("realbase");
        run_git(&root, &["init", "-q", "-b", "main"]);
        run_git(&root, &["commit", "-q", "--allow-empty", "-m", "init"]);
        run_git(&root, &["branch", "release/1.0"]);
        assert_eq!(
            resolve_followup_base(&root, &UpstreamRef::local("release/1.0")),
            Ok(UpstreamRef::local("release/1.0"))
        );
        assert_eq!(
            resolve_followup_base(&root, &UpstreamRef::local("main")),
            Ok(UpstreamRef::local("main"))
        );

        run_git(&root, &["branch", "-D", "release/1.0"]);
        assert!(
            resolve_followup_base(&root, &UpstreamRef::local("release/1.0")).is_err(),
            "a merge target deleted after the merge is reported, not replaced by the default"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_remote_tracking_merge_target_is_checked_on_that_remote() {
        let origin = temp_root("realorigin");
        run_git(&origin, &["init", "-q", "-b", "main"]);
        run_git(&origin, &["commit", "-q", "--allow-empty", "-m", "init"]);
        run_git(&origin, &["branch", "release/1.0"]);
        let root = temp_root("realclone");
        run_git(&root, &["init", "-q", "-b", "main"]);
        run_git(
            &root,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );

        let on_origin = |branch: &str| UpstreamRef::new("origin", branch);
        assert_eq!(
            resolve_followup_base(&root, &on_origin("main")),
            Ok(on_origin("main"))
        );
        assert_eq!(
            resolve_followup_base(&root, &on_origin("release/1.0")),
            Ok(on_origin("release/1.0"))
        );

        run_git(&origin, &["branch", "-D", "release/1.0"]);
        let err = resolve_followup_base(&root, &on_origin("release/1.0")).unwrap_err();
        assert!(
            err.contains("'origin'") && err.contains("'release/1.0'"),
            "{err}"
        );
        assert!(resolve_followup_base(&root, &on_origin("gone")).is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&origin);
    }

    #[test]
    fn task_prompt_carries_the_note_and_the_original_prompt() {
        let prompt = task_prompt("rev", &item("do the thing", Some("the original ask")));
        assert!(prompt.contains("do the thing"));
        assert!(prompt.contains("the original ask"));
        let without = task_prompt("rev", &item("do the thing", None));
        assert!(!without.contains("For context"));
    }

    #[test]
    fn task_prompt_tells_the_agent_to_commit_its_work() {
        let prompt = task_prompt("rev", &item("do the thing", None));
        assert!(prompt.contains("commit"), "{prompt}");
        assert!(prompt.contains("git push -u origin HEAD"), "{prompt}");
    }
}
