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
//!    submits it through the ordinary submit path, then records a blocking
//!    **waypoint** that explains the follow-up: the merged review is on its
//!    roster, the squad is its affected entry. The waypoint stays open until
//!    the squad finishes, so it reads as "follow-up outstanding".
//!
//! Drafting is deterministic: each task's prompt is the deferred note plus
//! the original prompt as context. There is no model call, so drafting has no
//! failure or cost mode of its own, and the whole flow runs without an agent.
//!
//! The squad's base branch is the branch the review merged into, or the
//! remote's default branch when that branch no longer exists.

use std::path::Path;
use std::process::Command;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::guardian::GuardianView;
use crate::store::{Result, Store, StoreError, now_ms};
use crate::waypoints::{AffectedMode, BearingDecision, WaypointEntryKind};

/// The offer has been sent and awaits an answer.
pub const STATUS_OFFERED: &str = "offered";
/// The user accepted; the squad and waypoint were created.
pub const STATUS_ACCEPTED: &str = "accepted";
/// The user declined; nothing was created.
pub const STATUS_DECLINED: &str = "declined";

/// The sentinel upstream meaning "the remote's default branch".
const DEFAULT_UPSTREAM: &str = "<<default>>";

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

/// One row of `followup_offers`.
#[derive(Debug, Clone, Serialize)]
pub struct FollowupOfferView {
    pub guardian_id: String,
    pub status: String,
    pub items: Vec<FollowupItem>,
    /// Follow-up generation of the review that triggered the offer (`0` for
    /// ordinary work).
    pub depth: i64,
    /// The waypoint an accepted offer created.
    pub waypoint_id: Option<String>,
    /// The squad an accepted offer created.
    pub squad_id: Option<String>,
    pub created_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
}

const OFFER_COLUMNS: &str =
    "guardian_id, status, items, depth, waypoint_id, squad_id, created_at_ms, resolved_at_ms";

fn map_offer_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FollowupOfferView> {
    let items: String = r.get(2)?;
    Ok(FollowupOfferView {
        guardian_id: r.get(0)?,
        status: r.get(1)?,
        // A malformed snapshot reads as empty rather than failing the whole
        // lookup: the offer row still has to be answerable (declinable).
        items: serde_json::from_str(&items).unwrap_or_default(),
        depth: r.get(3)?,
        waypoint_id: r.get(4)?,
        squad_id: r.get(5)?,
        created_at_ms: r.get(6)?,
        resolved_at_ms: r.get(7)?,
    })
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
             WHERE o.squad_id IS NOT NULL AND (
                 o.squad_id IN (SELECT squad_id FROM guardians WHERE id=?1)
                 OR o.squad_id IN (SELECT squad_id FROM cells WHERE review_guardian_id=?1)
             )",
            params![guardian_id],
            |r| r.get(0),
        )?;
        Ok(depth.unwrap_or(0))
    }

    /// The prompt, agent and model of the cell named by a prophecy's
    /// `entity_uri`, when that cell still exists.
    fn cell_context_for_uri(
        &self,
        entity_uri: &str,
    ) -> Option<(Option<String>, String, Option<String>)> {
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
        Some((cell.prompt, cell.agent, cell.model))
    }

    /// Offers follow-up work for the `deferred` prophecies of a review that
    /// just merged. Returns whether an offer was sent.
    ///
    /// Sends nothing -- and records nothing -- when the project turned offers
    /// off, when the `[followup]` config is invalid, when the review is already at the configured follow-up depth, or
    /// when it wrote no `deferred` prophecy. Once a row exists the review
    /// never offers again, so reopening it and re-merging is silent.
    pub fn maybe_offer_followups(&self, guardian_id: &str) -> Result<bool> {
        if self.get_followup_offer(guardian_id)?.is_some() {
            return Ok(false);
        }
        let guardian = self.get_guardian(guardian_id)?;
        let config = crate::config::load_followup_config(Path::new(&guardian.git_root));
        if !config.enabled() {
            return Ok(false);
        }
        if let Err(error) = config.validate() {
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
            .list_all_prophecies_for_guardian(guardian_id)?
            .into_iter()
            .filter(|p| p.kind == crate::prophecy::ProphecyKind::Deferred.as_str())
            .map(|p| {
                let context = self.cell_context_for_uri(&p.entity_uri);
                FollowupItem {
                    prophecy_id: p.id,
                    prompt: context
                        .as_ref()
                        .and_then(|(prompt, _, _)| prompt.as_deref())
                        .map(|prompt| truncate_chars(prompt, MAX_PROMPT_CHARS)),
                    agent: context.as_ref().map(|(_, agent, _)| agent.clone()),
                    model: context.and_then(|(_, _, model)| model),
                    entity_uri: p.entity_uri,
                    body: p.body,
                }
            })
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

    /// Claims an open offer for acceptance: flips `offered` to `accepted` in
    /// one statement, so two concurrent accepts cannot both proceed. Pair
    /// with [`Self::release_followup_claim`] if creating the squad fails.
    pub fn claim_followup_offer(&self, guardian_id: &str) -> Result<FollowupOfferView> {
        let changed = self.conn.execute(
            "UPDATE followup_offers SET status=?, resolved_at_ms=? WHERE guardian_id=? AND status=?",
            params![STATUS_ACCEPTED, now_ms(), guardian_id, STATUS_OFFERED],
        )?;
        self.answered_offer(guardian_id, changed, "accepted")
    }

    /// Puts a claimed offer back to `offered` after its squad could not be
    /// created, so the user can accept again.
    pub fn release_followup_claim(&self, guardian_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE followup_offers SET status=?, resolved_at_ms=NULL, waypoint_id=NULL, squad_id=NULL
             WHERE guardian_id=? AND status=?",
            params![STATUS_OFFERED, guardian_id, STATUS_ACCEPTED],
        )?;
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

    /// Records the squad and waypoint an accepted offer created.
    pub fn record_followup_result(
        &self,
        guardian_id: &str,
        squad_id: &str,
        waypoint_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE followup_offers SET squad_id=?, waypoint_id=? WHERE guardian_id=?",
            params![squad_id, waypoint_id, guardian_id],
        )?;
        Ok(())
    }

    /// Creates the blocking waypoint that explains a follow-up squad: the
    /// merged review is on its roster (the work it consists of, already
    /// landed), the squad is its blocking affected entry. The squad's bearing
    /// decision is recorded as `accepted` up front -- the follow-up *is* the
    /// work the waypoint describes, so there is nothing for it to answer, and
    /// leaving it unanswered would hold the squad and its review forever.
    /// Returns the waypoint id.
    pub fn create_followup_waypoint(
        &self,
        guardian: &GuardianView,
        squad_id: &str,
        items: &[FollowupItem],
    ) -> Result<String> {
        let waypoint_id = self.next_id("waypoint_seq", "waypoint")?;
        let label = format!("follow-up: {}", guardian.name);
        self.create_waypoint(
            &waypoint_id,
            Some(&label),
            &waypoint_prompt(&guardian.id, &guardian.name, items),
            None,
            None,
            false,
        )?;
        self.add_roster_entry(
            &waypoint_id,
            WaypointEntryKind::Review,
            &guardian.id,
            Some("the review the deferred work came out of; it has merged"),
        )?;
        self.add_affected_entry(
            &waypoint_id,
            WaypointEntryKind::Squad,
            squad_id,
            AffectedMode::Block,
        )?;
        self.set_affected_bearing_decision(
            &waypoint_id,
            WaypointEntryKind::Squad,
            squad_id,
            BearingDecision::Accepted,
        )?;
        crate::cartographer::Note::new("followup")
            .scope("followup")
            .guardian(&guardian.id)
            .squad(squad_id)
            .emit(
                self,
                format!(
                    "waypoint {waypoint_id} created for follow-up squad {squad_id} (review {})",
                    guardian.id
                ),
                serde_json::json!({"waypoint_id": waypoint_id, "squad_id": squad_id}),
            );
        Ok(waypoint_id)
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

/// The waypoint's prompt: what was deferred and where it came from, concise.
#[must_use]
pub fn waypoint_prompt(guardian_id: &str, review_name: &str, items: &[FollowupItem]) -> String {
    let mut out = format!(
        "Follow-up work deferred while building review {guardian_id} ({review_name}), which has \
         since merged. The deferred items:\n"
    );
    for item in items {
        out.push_str("- ");
        out.push_str(&truncate_chars(&item.body, MAX_MESSAGE_NOTE_CHARS));
        out.push_str(&format!(" (from {})\n", item.entity_uri));
    }
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
         merged; build on the current state of the branch.",
    );
    out
}

/// Drafts the follow-up squad's task file: one task per deferred item, all
/// feeding one review that targets `base`.
#[must_use]
pub fn draft_followup_toml(
    project: &str,
    guardian: &GuardianView,
    base: &str,
    items: &[FollowupItem],
) -> String {
    let review_key = "followup";
    let sentinel = format!("<<ralphus:new-review/{review_key}>>");
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
                format!("<<ralphus:new-worktree/{branch}?upstream={base}>>").into(),
            );
            cell.insert("prompt".into(), task_prompt(&guardian.name, item).into());
            cell.insert("review".into(), sentinel.clone().into());
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
    let mut review = toml::Table::new();
    review.insert(
        "id".into(),
        format!("ralphus:new-review/{review_key}").into(),
    );
    review.insert("upstream".into(), base.into());
    if let Some(machine) = machine {
        review.insert("machine".into(), machine.into());
    }
    let mut root = toml::Table::new();
    root.insert("task".into(), toml::Value::Array(tasks));
    root.insert("review".into(), toml::Value::Array(vec![review.into()]));
    toml::to_string(&root).unwrap_or_default()
}

/// Splits a stored merge target into `(remote, branch)`. The daemon records a
/// review's base either as a bare branch (`main`) or in remote-tracking form
/// (`origin/main`), so a leading segment naming a configured remote is the
/// remote, and anything else is a branch on `origin`.
fn split_remote(root: &Path, name: &str) -> (String, String) {
    if let Some((head, rest)) = name.split_once('/') {
        let is_remote = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["remote"])
            .output()
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .any(|remote| remote.trim() == head)
            })
            .unwrap_or(false);
        if is_remote {
            return (head.to_string(), rest.to_string());
        }
    }
    ("origin".to_string(), name.to_string())
}

/// Whether `git` finds `name` on its remote, falling back to the local refs
/// when the remote cannot be asked (no such remote, offline).
fn branch_exists(root: &Path, name: &str) -> bool {
    let (remote, branch) = split_remote(root, name);
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
    };
    let heads = format!("refs/heads/{branch}");
    if let Ok(out) = git(&["ls-remote", "--exit-code", "--heads", &remote, &heads]) {
        match out.status.code() {
            Some(0) => return true,
            // `--exit-code`: 2 means the remote answered and has no such ref.
            Some(2) => return false,
            _ => {}
        }
    }
    let remote_tracking = format!("refs/remotes/{remote}/{branch}");
    [heads, remote_tracking].iter().any(|reference| {
        git(&["rev-parse", "--verify", "--quiet", reference])
            .map(|out| out.status.success())
            .unwrap_or(false)
    })
}

/// The upstream a follow-up squad is based on: the branch the review merged
/// into, or the remote's default branch when that branch is gone.
#[must_use]
pub fn resolve_followup_base(git_root: &Path, merged_into: &str) -> String {
    if !merged_into.is_empty() && branch_exists(git_root, merged_into) {
        merged_into.to_string()
    } else {
        DEFAULT_UPSTREAM.to_string()
    }
}

/// What accepting an offer created.
#[derive(Debug, Clone, Serialize)]
pub struct AcceptResult {
    pub squad_id: String,
    pub waypoint_id: String,
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
/// ordinary submit path (so every preflight applies), and creates the
/// waypoint that explains it.
///
/// The offer is claimed first, atomically, so a second accept -- a retried
/// request, a double click -- finds it already accepted and creates nothing.
/// A failure before the squad exists puts the claim back so the user can try
/// again; a failure after it leaves the offer accepted, since a second squad
/// would duplicate the first.
pub(crate) fn accept_offer(
    daemon: &crate::server::Daemon,
    guardian_id: &str,
) -> std::result::Result<AcceptResult, AcceptError> {
    let (offer, guardian) = {
        let store = daemon.lock();
        let guardian = store.get_guardian(guardian_id)?;
        let offer = store.claim_followup_offer(guardian_id)?;
        (offer, guardian)
    };
    let (squad_id, base, started) = match submit_followup_squad(daemon, &offer, &guardian) {
        Ok(created) => created,
        Err(e) => {
            let _ = daemon.lock().release_followup_claim(guardian_id);
            return Err(e);
        }
    };
    let store = daemon.lock();
    store.record_followup_result(guardian_id, &squad_id, "")?;
    let waypoint_id = store.create_followup_waypoint(&guardian, &squad_id, &offer.items)?;
    store.record_followup_result(guardian_id, &squad_id, &waypoint_id)?;
    Ok(AcceptResult {
        squad_id,
        waypoint_id,
        base,
        started,
    })
}

/// Drafts and submits the squad; returns `(squad_id, base, started)`.
fn submit_followup_squad(
    daemon: &crate::server::Daemon,
    offer: &FollowupOfferView,
    guardian: &GuardianView,
) -> std::result::Result<(String, String, bool), AcceptError> {
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
    let config = crate::config::load_followup_config(root);
    let base = resolve_followup_base(root, &guardian.base_branch);
    let started = config.auto_start();
    let body = serde_json::json!({
        "toml": draft_followup_toml(&project, guardian, &base, &offer.items),
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

    /// A review whose git root is a throwaway directory, so a test can drop a
    /// `.ralphus.toml` next to it.
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
    fn a_project_can_turn_offers_off() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("off");
        std::fs::write(root.join(".ralphus.toml"), "[followup]\nenabled = false\n").unwrap();
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "later");
        merge(&s, "g1");
        assert!(s.get_followup_offer("g1").unwrap().is_none());
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

        // Raising the cap for the project lets the second generation offer.
        std::fs::write(root.join(".ralphus.toml"), "[followup]\nmax_depth = 2\n").unwrap();
        s.maybe_offer_followups("g2").unwrap();
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
        std::fs::write(root.join(".ralphus.toml"), "[followup]\nmax_depth = 0\n").unwrap();
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "later");
        merge(&s, "g1");
        assert!(
            s.get_followup_offer("g1").unwrap().is_none(),
            "max_depth = 0 while enabled is rejected, so nothing is offered"
        );

        // Nothing was recorded, so fixing the config lets a later merge offer.
        std::fs::write(root.join(".ralphus.toml"), "[followup]\nmax_depth = 1\n").unwrap();
        assert!(s.maybe_offer_followups("g1").unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_offer_can_be_claimed_only_once_and_released_on_failure() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("claim");
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "later");
        merge(&s, "g1");

        assert_eq!(
            s.claim_followup_offer("g1").unwrap().status,
            STATUS_ACCEPTED
        );
        assert!(matches!(
            s.claim_followup_offer("g1"),
            Err(StoreError::InvalidTransition(_))
        ));
        assert!(matches!(
            s.decline_followup_offer("g1"),
            Err(StoreError::InvalidTransition(_))
        ));

        s.release_followup_claim("g1").unwrap();
        assert_eq!(
            s.get_followup_offer("g1").unwrap().unwrap().status,
            STATUS_OFFERED
        );
        assert_eq!(
            s.decline_followup_offer("g1").unwrap().status,
            STATUS_DECLINED
        );
        assert!(matches!(
            s.claim_followup_offer("nope"),
            Err(StoreError::NotFound)
        ));
        let _ = std::fs::remove_dir_all(&root);
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
            "release/1.0",
            &[
                item("add the cache", Some("the original ask")),
                item("document \"the\" flag", None),
            ],
        );
        let report = ralphus_core::validate::validate_toml(&toml);
        assert!(report.errors.is_empty(), "{:?}\n{toml}", report.errors);
        let file: ralphus_core::schema::TaskFile = toml::from_str(&toml).unwrap();
        assert_eq!(file.task.len(), 2, "one task per deferred item");
        assert_eq!(file.review.len(), 1);
        assert_eq!(file.review[0].upstream.as_deref(), Some("release/1.0"));
        let cell = &file.task[0].cell[0];
        assert!(
            cell.cwd
                .as_deref()
                .unwrap()
                .contains("upstream=release/1.0")
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
        let toml = draft_followup_toml("proj", &guardian, "main", &[item("later", None)]);
        let file: ralphus_core::schema::TaskFile = toml::from_str(&toml).unwrap();
        assert_eq!(file.task[0].machine.as_deref(), Some("loopback:exercise"));
        assert_eq!(file.review[0].machine.as_deref(), Some("loopback:exercise"));

        let local = draft_followup_toml(
            "proj",
            &{
                s.conn
                    .execute("UPDATE guardians SET machine=NULL WHERE id='g1'", [])
                    .unwrap();
                s.get_guardian("g1").unwrap()
            },
            "main",
            &[item("later", None)],
        );
        let file: ralphus_core::schema::TaskFile = toml::from_str(&local).unwrap();
        assert_eq!(file.task[0].machine, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_base_branch_falls_back_to_the_default_sentinel() {
        let root = temp_root("base");
        // Not a repository at all: nothing to find the branch in.
        assert_eq!(resolve_followup_base(&root, "gone"), DEFAULT_UPSTREAM);
        assert_eq!(resolve_followup_base(&root, ""), DEFAULT_UPSTREAM);
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
    fn the_base_is_the_merge_target_while_it_exists_and_the_default_once_it_is_gone() {
        let root = temp_root("realbase");
        run_git(&root, &["init", "-q", "-b", "main"]);
        run_git(&root, &["commit", "-q", "--allow-empty", "-m", "init"]);
        run_git(&root, &["branch", "release/1.0"]);
        assert_eq!(resolve_followup_base(&root, "release/1.0"), "release/1.0");
        assert_eq!(resolve_followup_base(&root, "main"), "main");

        run_git(&root, &["branch", "-D", "release/1.0"]);
        assert_eq!(
            resolve_followup_base(&root, "release/1.0"),
            DEFAULT_UPSTREAM,
            "a merge target deleted after the merge falls back to the remote default"
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

        // The daemon stores a review's base as `origin/main`, not `main`.
        assert_eq!(resolve_followup_base(&root, "origin/main"), "origin/main");
        assert_eq!(resolve_followup_base(&root, "main"), "main");
        assert_eq!(
            resolve_followup_base(&root, "origin/release/1.0"),
            "origin/release/1.0"
        );

        run_git(&origin, &["branch", "-D", "release/1.0"]);
        assert_eq!(
            resolve_followup_base(&root, "origin/release/1.0"),
            DEFAULT_UPSTREAM,
            "a base deleted on the remote after the merge falls back to the default"
        );
        assert_eq!(
            resolve_followup_base(&root, "origin/gone"),
            DEFAULT_UPSTREAM
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&origin);
    }

    #[test]
    fn the_follow_up_waypoint_lists_the_review_as_roster_and_the_squad_as_blocking() {
        use crate::prophecy::ProphecyKind;
        let s = store();
        let root = temp_root("waypoint");
        seed_guardian(&s, "g1", &root);
        defer(&s, "g1", ProphecyKind::Deferred, "later");
        merge(&s, "g1");
        let offer = s.get_followup_offer("g1").unwrap().unwrap();
        let guardian = s.get_guardian("g1").unwrap();

        let waypoint_id = s
            .create_followup_waypoint(&guardian, "squad-f", &offer.items)
            .unwrap();
        assert!(s.waypoint_is_open(&waypoint_id).unwrap());
        let roster = s.list_roster_entries(&waypoint_id).unwrap();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].entry_id, "g1");
        assert!(roster[0].terminal, "the merged review has already landed");
        let affected = s.list_affected_entries(&waypoint_id).unwrap();
        assert_eq!(affected.len(), 1);
        assert_eq!(affected[0].entry_id, "squad-f");
        assert_eq!(affected[0].mode, AffectedMode::Block);
        assert_eq!(
            affected[0].bearing_decision,
            Some(BearingDecision::Accepted)
        );
        assert!(
            s.squad_block_gating_waypoint("squad-f").unwrap().is_none(),
            "an answered entry on a landed roster must not hold the squad"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn task_prompt_carries_the_note_and_the_original_prompt() {
        let prompt = task_prompt("rev", &item("do the thing", Some("the original ask")));
        assert!(prompt.contains("do the thing"));
        assert!(prompt.contains("the original ask"));
        let without = task_prompt("rev", &item("do the thing", None));
        assert!(!without.contains("For context"));
    }
}
