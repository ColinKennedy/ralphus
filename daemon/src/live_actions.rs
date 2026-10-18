//! Live actions: agent-run review actions that test a review's tip live.
//!
//! A live action is a prompt an agent executes against the review's prepared
//! checkout -- launching the product, driving it, and reporting what it saw.
//! Its prompts come from the live handoffs member cells posted
//! (`RALPHUS_LIVE_ACTION:`), deduplicated once per review; a user action whose
//! `prompt` is set runs through the same executor. See `docs/glossary.md`
//! ("live action", "live run", "live ref").
//!
//! This module owns one live run's execution: holding the checkout, running
//! the agent through the review's runner (a plain child process whose durable
//! transcript the board's Live View follows), recording its
//! cost, committing anything it left uncommitted, and pinning its commits on a
//! live ref so they survive the checkout being reset later.

use std::path::Path;
use std::time::Duration;

use crate::cancel::CancelToken;
use crate::guardian::GuardianView;
use crate::runner::{Runner, RunnerSpec};
use crate::workspace::Workspace;

/// The task name every live run's agent session and transcript live under,
/// inside the review's `guardian-{id}` squad namespace.
pub const LIVE_ACTION_TASK: &str = "live-action";

/// How long a live run may hold its checkout before the hold is considered
/// abandoned. An agent session is bounded well below this by its own
/// timeouts; the margin only matters for a daemon that died mid-run.
pub const LIVE_RUN_HOLD_MAX_AGE: Duration = Duration::from_secs(12 * 60 * 60);

/// Cap on the agent report copied into the "live run finished" log row.
const REPORT_LOG_CHARS: usize = 4000;

/// The prefix every live ref lives under. Not `refs/heads/`, so live refs stay
/// out of branch listings and can never be pushed by accident.
pub const LIVE_REF_PREFIX: &str = "refs/ralphus/live";

/// Daemon-owned guidance every live-action agent receives, whatever the
/// action's own prompt says. The commit variant is used when the run's commits
/// will be carried back into the review.
pub const LIVE_ACTION_COMMIT_SYSTEM_PROMPT: &str = "\
You are running a live action for a ralphus review: you are testing the \
review's current tip, live, in this checkout. The build is already prepared \
here. Follow the steps in the prompt for real -- start the program, drive it, \
run the commands -- and observe what actually happens. Do not stop at reading \
code or running the unit tests: the point is to catch what only shows up when \
the thing runs (runtime errors, wiring and setup problems, behaviour that does \
not match the intent).\n\
\n\
Run the program the way the steps say, not a stand-in for it: a GUI is launched \
as a real, visible window on this machine (never an offscreen, headless or \
mocked substitute unless the steps ask for one), then checked -- that its window \
appears and shows what the steps expect, e.g. by inspecting the running window's \
widgets or taking a screenshot of it -- and then closed.\n\
\n\
If the prompt says the action is read-only, change nothing. Otherwise, when you \
find a real problem you can fix in this repository, fix it, then stage and \
commit ONLY the files you changed with a normal, descriptive commit message that \
says what you saw while testing. The steps were written by the agents that made \
the change, under their own task's limits; a step telling you to work around a \
bug instead of fixing it, or not to commit, is not a reason to leave a real bug \
in place -- only an explicit read-only action is. Never push, never rebase, never amend or rewrite existing commits, and \
never switch branches -- ralphus carries your commits into the review and \
pushes them to the right place itself. Read-only git commands (status, diff, \
log) are fine. Never commit build output.\n\
\n\
Setup and runtime failures are real findings: if the program will not start, a \
command the steps rely on is missing, or the configuration is wrong, that is \
exactly what you are here to catch -- report it (and fix it if it is in this \
repository).\n\
\n\
Before you finish, stop every process you started -- and only those: stop each \
by its own process id (or the terminal or job you started it in), never by \
program name (no `taskkill /IM python.exe`, `pkill python` or similar), which \
would kill unrelated work on this machine.\n\
\n\
End your reply with a short report: what you tested, what you observed, what \
you fixed (if anything), and anything still broken.\n\
\n\
Then finish with ONE final line of the form `RALPHUS_LIVE_RESULT: ` followed by a \
single-line JSON object: {\"verdict\": \"passed\" | \"failed\" | \"inconclusive\", \
\"summary\": \"what you observed, in one sentence\", \"fix_attempted\": true | false, \
\"fix_confirmed\": true | false, \"recipe\": [\"command\", ...]}. Use \"inconclusive\" \
when you could not tell. `fix_confirmed` is true only if you re-ran the failing step \
after your fix and saw it work. `recipe` lists the shell commands that reproduce what \
you tested. A recipe command may leave out directory navigation (`cd`, `Push-Location`, \
...), output-only filters (`| head`, `| rg`, `| tee`, `| Select-Object`, `| findstr`, \
...), redirection (`2>&1`, `> file`) and process wrappers (`timeout`, `nice`, `time`, \
`bash -c`), but must keep the meaningful command, its arguments and its environment \
settings (`MODE=production npm test`, not `npm test`); input-changing forms \
(`< file`, `yes |`), `&`, `|| true` and sudo are part of the command. Setup commands \
(`source .venv/bin/activate`, `set -e`, `nvm use 20`) may be left out, but if you \
include one, write it exactly as run. \
Example: recorded `cd app && npm test 2>&1 | tee test.log | rg \\\"failed|error\\\"`, recipe \
`npm test`. ralphus keeps only commands it recorded you running.";

/// The read-only variant: the run's edits are never carried back.
pub const LIVE_ACTION_READ_ONLY_SYSTEM_PROMPT: &str = "\
You are running a live action for a ralphus review: you are testing the \
review's current tip, live, in this checkout. The build is already prepared \
here. Follow the steps in the prompt for real -- start the program, drive it, \
run the commands -- and observe what actually happens. Do not stop at reading \
code or running the unit tests: the point is to catch what only shows up when \
the thing runs (runtime errors, wiring and setup problems, behaviour that does \
not match the intent). Run the program the way the steps say: a GUI is launched \
as a real, visible window (never an offscreen or headless substitute unless the \
steps ask for one), checked, then closed.\n\
\n\
This run is read-only: do not edit, stage or commit anything, and never push. \
Setup and runtime failures are real findings: report them. Before you finish, \
stop every process you started -- and only those, each by its own process id, \
never by program name (no `taskkill /IM python.exe`, `pkill python` or \
similar).\n\
\n\
End your reply with a short report: what you tested, what you observed, and \
anything broken (with the exact error and where it comes from).\n\
\n\
Then finish with ONE final line of the form `RALPHUS_LIVE_RESULT: ` followed by a \
single-line JSON object: {\"verdict\": \"passed\" | \"failed\" | \"inconclusive\", \
\"summary\": \"what you observed, in one sentence\", \"fix_attempted\": false, \
\"fix_confirmed\": false, \"recipe\": [\"command\", ...]}. Use \"inconclusive\" when \
you could not tell. `recipe` lists the shell commands that reproduce what you tested. \
A recipe command may leave out directory navigation (`cd`, `Push-Location`, ...), \
output-only filters (`| head`, `| rg`, `| tee`, `| Select-Object`, `| findstr`, ...) \
redirection (`2>&1`, `> file`) and process wrappers (`timeout`, `nice`, `time`, \
`bash -c`), but must keep the meaningful command, its arguments and its environment \
settings (`MODE=production npm test`, not `npm test`); input-changing forms \
(`< file`, `yes |`), `&`, `|| true` and sudo are part of the command. Setup commands \
(`source .venv/bin/activate`, `set -e`, `nvm use 20`) may be left out, but if you \
include one, write it exactly as run. \
Example: recorded `Push-Location app; dotnet test > results.txt; Pop-Location`, recipe \
`dotnet test`. ralphus keeps only commands it recorded you running.";

/// The marker a cell's agent writes a live handoff under.
pub const LIVE_ACTION_MARKER: &str = "RALPHUS_LIVE_ACTION:";

/// The explicit "this cannot be tested live" answer.
const NOTHING_TO_TEST_LIVE: &str = "(nothing to test live)";

/// Longest single handoff kept; longer text is cut.
const HANDOFF_MAX_CHARS: usize = 4000;

/// Most handoffs kept from one cell.
const MAX_HANDOFFS_PER_CELL: usize = 8;

/// Appended to every prompt cell's system prompt (`RunnerSpec::from_row`): the
/// question whose answer becomes the review's live actions.
pub const LIVE_HANDOFF_SYSTEM_PROMPT: &str = "## Live Test Handoff\n\
Operational note, not a change to your task: after this work is merged into a \
review, ralphus may test it live with a separate agent that has not seen this \
conversation. When you finish, and before any RALPHUS_GHOST section, say how \
you would test this change live if you had to: run the real program the way a \
user would, exercise what you changed, and check the result. Write each test on \
its own, starting on a new line with the exact marker 'RALPHUS_LIVE_ACTION:' \
followed by a short title for the test (a few words, e.g. 'GUI shows the Ready \
label') on that same line, then on the next lines concrete steps -- which program \
or entry point, the exact commands, \
any setup it needs (ports, data, configuration), what to do, and what correct \
looks like. Describe the test only: do not pass on your own task's restrictions \
(files you were told not to touch, things not to commit) -- whoever runs the test \
decides what to fix. A test may span several lines; it ends at the next line that \
starts with a RALPHUS_ marker. Write one per distinct thing worth testing (for example \
one per product you touched). If this change cannot be meaningfully tested \
live (a pure refactor, documentation only), write \
'RALPHUS_LIVE_ACTION: (nothing to test live)'.";

/// The live handoffs one agent reply carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveHandoffs {
    /// The reply has no `RALPHUS_LIVE_ACTION:` marker at all.
    NoMarker,
    /// The agent said there is nothing to test live.
    NothingToTest,
    /// One or more test descriptions, in reply order.
    Tests(Vec<String>),
}

/// Extract every `RALPHUS_LIVE_ACTION:` handoff from an agent's reply. Each
/// marker starts one handoff, which runs until the next line that starts with
/// a `RALPHUS_` marker (another handoff, the ghost, a prophecy) or the end of
/// the reply.
#[must_use]
pub fn parse_live_handoffs(reply: &str) -> LiveHandoffs {
    fn finish(current: &mut Option<String>, tests: &mut Vec<String>) {
        if let Some(text) = current.take() {
            let text = text.trim();
            if !text.is_empty() && !text.eq_ignore_ascii_case(NOTHING_TO_TEST_LIVE) {
                tests.push(text.chars().take(HANDOFF_MAX_CHARS).collect());
            }
        }
    }
    let mut tests: Vec<String> = Vec::new();
    let mut saw_marker = false;
    let mut current: Option<String> = None;
    for line in reply.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(LIVE_ACTION_MARKER) {
            finish(&mut current, &mut tests);
            saw_marker = true;
            current = Some(rest.trim().to_string());
        } else if trimmed.starts_with("RALPHUS_") {
            finish(&mut current, &mut tests);
        } else if let Some(text) = current.as_mut() {
            text.push('\n');
            text.push_str(line);
        }
    }
    finish(&mut current, &mut tests);
    tests.truncate(MAX_HANDOFFS_PER_CELL);
    match (saw_marker, tests.is_empty()) {
        (false, _) => LiveHandoffs::NoMarker,
        (true, true) => LiveHandoffs::NothingToTest,
        (true, false) => LiveHandoffs::Tests(tests),
    }
}

/// One cell's stored live handoffs.
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct CellLiveHandoffs {
    /// The test descriptions, in the order the agent wrote them.
    pub tests: Vec<String>,
    /// The agent said there is nothing to test live.
    pub nothing_to_test: bool,
    /// When they were recorded; `None` when the cell never posted any.
    pub recorded_at_ms: Option<i64>,
}

impl crate::store::Store {
    /// Replace a cell's live handoffs with the ones from its latest reply.
    /// A reply with no marker at all leaves nothing stored for the cell.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn replace_cell_live_handoffs(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        handoffs: &LiveHandoffs,
    ) -> crate::store::Result<()> {
        let now = crate::store::now_ms();
        self.conn.execute(
            "DELETE FROM cell_live_handoffs WHERE squad_id=? AND task_idx=? AND cell_idx=?",
            rusqlite::params![squad_id, task_idx, cell_idx],
        )?;
        let rows: Vec<(String, bool)> = match handoffs {
            LiveHandoffs::NoMarker => Vec::new(),
            LiveHandoffs::NothingToTest => vec![(String::new(), true)],
            LiveHandoffs::Tests(tests) => tests.iter().map(|t| (t.clone(), false)).collect(),
        };
        for (ordinal, (text, nothing)) in rows.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO cell_live_handoffs
                    (squad_id, task_idx, cell_idx, ordinal, text, nothing_to_test, created_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    squad_id,
                    task_idx,
                    cell_idx,
                    i64::try_from(ordinal).unwrap_or(i64::MAX),
                    text,
                    i64::from(*nothing),
                    now
                ],
            )?;
        }
        Ok(())
    }

    /// A cell's stored live handoffs.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn cell_live_handoffs(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> crate::store::Result<CellLiveHandoffs> {
        cell_live_handoffs_conn(&self.conn, squad_id, task_idx, cell_idx)
    }
}

/// [`crate::store::Store::cell_live_handoffs`] over a bare connection, for
/// read-pool callers.
///
/// # Errors
///
/// On a database failure.
pub fn cell_live_handoffs_conn(
    conn: &rusqlite::Connection,
    squad_id: &str,
    task_idx: i64,
    cell_idx: i64,
) -> crate::store::Result<CellLiveHandoffs> {
    let mut stmt = conn.prepare(
        "SELECT text, nothing_to_test, created_at_ms FROM cell_live_handoffs
         WHERE squad_id=? AND task_idx=? AND cell_idx=? ORDER BY ordinal",
    )?;
    let rows = stmt.query_map(rusqlite::params![squad_id, task_idx, cell_idx], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? != 0,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = CellLiveHandoffs::default();
    for row in rows {
        let (text, nothing, at) = row?;
        out.recorded_at_ms = Some(at);
        if nothing {
            out.nothing_to_test = true;
        } else {
            out.tests.push(text);
        }
    }
    Ok(out)
}

// ---- a review's member cells and their handoffs ----------------------------

/// Cell states that count as finished for live actions: nothing more will
/// change the review's tip because of this cell.
const FINISHED_CELL_STATES: [&str; 4] = ["done", "failed", "cancelled", "ignored"];

/// One cell that feeds a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberCell {
    pub squad_id: String,
    pub task_idx: i64,
    pub cell_idx: i64,
    pub state: String,
    pub finished_at_ms: Option<i64>,
}

impl MemberCell {
    /// Whether this cell has finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        FINISHED_CELL_STATES.contains(&self.state.as_str())
    }

    /// The cell's stable URI, the same key its ghost lives under.
    #[must_use]
    pub fn uri(&self) -> String {
        crate::ghost::cell_uri(&self.squad_id, self.task_idx, self.cell_idx)
    }
}

/// Every cell that feeds review `guardian_id`: cells recorded against it at
/// submission, plus (for cells with no recorded review) cells whose branch is
/// one of its branches -- the same membership rule the scheduler's
/// stack-readiness gating uses.
///
/// # Errors
///
/// On a database failure.
pub fn review_member_cells_conn(
    conn: &rusqlite::Connection,
    guardian_id: &str,
) -> crate::store::Result<Vec<MemberCell>> {
    let mut stmt = conn.prepare(
        "SELECT s.squad_id, s.task_idx, s.idx, s.state, s.finished_at_ms FROM cells s
         WHERE s.review_guardian_id = ?1
            OR (
                s.review_guardian_id IS NULL
                AND EXISTS (
                    SELECT 1 FROM guardian_branches gb
                    WHERE gb.guardian_id = ?1 AND gb.branch = s.review_branch
                )
            )
         ORDER BY s.squad_id, s.task_idx, s.idx",
    )?;
    let rows = stmt.query_map(rusqlite::params![guardian_id], |r| {
        Ok(MemberCell {
            squad_id: r.get(0)?,
            task_idx: r.get(1)?,
            cell_idx: r.get(2)?,
            state: r.get(3)?,
            finished_at_ms: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// One handoff selected for a review, with the cell it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedHandoff {
    pub source_cell: String,
    pub text: String,
}

/// The handoffs that seed a review's live actions: for each task among the
/// review's member cells, only the latest-finished cell's (a later phase of a
/// split-up unit of work replaces an earlier one's).
///
/// # Errors
///
/// On a database failure.
pub fn collect_review_handoffs_conn(
    conn: &rusqlite::Connection,
    guardian_id: &str,
) -> crate::store::Result<Vec<CollectedHandoff>> {
    let members = review_member_cells_conn(conn, guardian_id)?;
    let mut latest: std::collections::BTreeMap<(String, i64), MemberCell> =
        std::collections::BTreeMap::new();
    for cell in members.into_iter().filter(MemberCell::is_finished) {
        let key = (cell.squad_id.clone(), cell.task_idx);
        let replace = latest.get(&key).is_none_or(|current| {
            (cell.finished_at_ms, cell.cell_idx) > (current.finished_at_ms, current.cell_idx)
        });
        if replace {
            latest.insert(key, cell);
        }
    }
    let mut out = Vec::new();
    for cell in latest.values() {
        let handoffs = cell_live_handoffs_conn(conn, &cell.squad_id, cell.task_idx, cell.cell_idx)?;
        for text in handoffs.tests {
            out.push(CollectedHandoff {
                source_cell: cell.uri(),
                text,
            });
        }
    }
    Ok(out)
}

// ---- a review's live actions ------------------------------------------------

/// One live action of a review: a deduplicated live test.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct LiveAction {
    pub idx: i64,
    pub label: String,
    pub prompt: String,
    /// URIs of the cells whose handoffs this action came from.
    pub source_cells: Vec<String>,
    pub created_at_ms: i64,
}

/// A live action about to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLiveAction {
    pub label: String,
    pub prompt: String,
    pub source_cells: Vec<String>,
}

/// A review's live actions, in order.
///
/// # Errors
///
/// On a database failure.
pub fn guardian_live_actions_conn(
    conn: &rusqlite::Connection,
    guardian_id: &str,
) -> crate::store::Result<Vec<LiveAction>> {
    let mut stmt = conn.prepare(
        "SELECT idx, label, prompt, source_cells, created_at_ms FROM guardian_live_actions
         WHERE guardian_id=? ORDER BY idx",
    )?;
    let rows = stmt.query_map(rusqlite::params![guardian_id], |r| {
        let sources: String = r.get(3)?;
        Ok(LiveAction {
            idx: r.get(0)?,
            label: r.get(1)?,
            prompt: r.get(2)?,
            source_cells: serde_json::from_str(&sources).unwrap_or_default(),
            created_at_ms: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

impl crate::store::Store {
    /// A review's live actions, in order.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn guardian_live_actions(
        &self,
        guardian_id: &str,
    ) -> crate::store::Result<Vec<LiveAction>> {
        guardian_live_actions_conn(&self.conn, guardian_id)
    }

    /// Store a review's live actions -- once. The set is fixed after it is
    /// first written; a review that already has live actions keeps them and
    /// this returns `false`.
    ///
    /// # Errors
    ///
    /// On a database failure.
    pub fn insert_guardian_live_actions(
        &self,
        guardian_id: &str,
        actions: &[NewLiveAction],
    ) -> crate::store::Result<bool> {
        let existing: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM guardian_live_actions WHERE guardian_id=?",
            rusqlite::params![guardian_id],
            |r| r.get(0),
        )?;
        if existing > 0 {
            return Ok(false);
        }
        let now = crate::store::now_ms();
        for (idx, action) in actions.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO guardian_live_actions
                    (guardian_id, idx, label, prompt, source_cells, created_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    guardian_id,
                    i64::try_from(idx).unwrap_or(i64::MAX),
                    action.label,
                    action.prompt,
                    serde_json::to_string(&action.source_cells).unwrap_or_else(|_| "[]".into()),
                    now
                ],
            )?;
            // Attribution is frozen here: later cell moves never rewrite it.
            let idx = i64::try_from(idx).unwrap_or(i64::MAX);
            for branch_id in crate::live_results::resolve_branch_ids(
                &self.conn,
                guardian_id,
                &action.source_cells,
            ) {
                self.conn.execute(
                    "INSERT OR IGNORE INTO guardian_live_action_branches
                        (guardian_id, action_idx, branch_id) VALUES (?, ?, ?)",
                    rusqlite::params![guardian_id, idx, branch_id],
                )?;
            }
        }
        Ok(true)
    }
}

/// A short button label for a handoff that dedupe did not name: its first
/// line, cut to a readable length.
fn fallback_label(text: &str) -> String {
    const MAX: usize = 60;
    let first = text.lines().next().unwrap_or_default().trim();
    if first.chars().count() <= MAX {
        first.to_string()
    } else {
        let cut: String = first.chars().take(MAX - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

/// One action per handoff, used when deduplication is unnecessary (a single
/// handoff) or the deduplicating agent's answer could not be used.
#[must_use]
pub fn undeduplicated(handoffs: &[CollectedHandoff]) -> Vec<NewLiveAction> {
    handoffs
        .iter()
        .map(|h| NewLiveAction {
            label: fallback_label(&h.text),
            prompt: h.text.clone(),
            source_cells: vec![h.source_cell.clone()],
        })
        .collect()
}

/// The instructions for the deduplicating agent.
const DEDUPE_SYSTEM_PROMPT: &str = "You merge live-test descriptions for a code \
review. You will be given numbered descriptions, each written by an agent that \
changed part of the code, of how to test its change live. Several may describe \
the same test (the same product, the same setup, overlapping checks). Merge \
those into one test that keeps every distinct check from each, and keep tests \
of different things separate -- do not force everything into one. Keep the \
concrete steps: commands, setup, what to check. Do not invent steps no \
description asked for. Answer with ONLY a JSON object, no prose and no code \
fence, of the form {\"live_actions\": [{\"label\": \"<short button label, at \
most 60 characters>\", \"prompt\": \"<the merged steps>\", \"sources\": [<the \
numbers of every description merged into this one>]}]}. Every description \
number must appear in exactly one entry's sources. Do not edit any file.";

#[derive(serde::Deserialize)]
struct DedupeAnswer {
    live_actions: Vec<DedupeEntry>,
}

#[derive(serde::Deserialize)]
struct DedupeEntry {
    label: String,
    prompt: String,
    #[serde(default)]
    sources: Vec<usize>,
}

/// Parse the deduplicating agent's answer against the handoffs it was given.
/// `None` when the answer is unusable: not JSON, empty, a source number out of
/// range, or a handoff left out -- the caller then keeps one action per
/// handoff rather than silently dropping a test.
#[must_use]
pub fn parse_dedupe_answer(
    reply: &str,
    handoffs: &[CollectedHandoff],
) -> Option<Vec<NewLiveAction>> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    let answer: DedupeAnswer = serde_json::from_str(reply.get(start..=end)?).ok()?;
    if answer.live_actions.is_empty() {
        return None;
    }
    let mut covered = vec![false; handoffs.len()];
    let mut out = Vec::new();
    for entry in answer.live_actions {
        let prompt = entry.prompt.trim();
        if prompt.is_empty() || entry.sources.is_empty() {
            return None;
        }
        let mut sources: Vec<String> = Vec::new();
        for n in entry.sources {
            // The agent is shown 1-based numbers.
            let i = n.checked_sub(1)?;
            let handoff = handoffs.get(i)?;
            if std::mem::replace(&mut covered[i], true) {
                // A handoff number may appear exactly once across the answer.
                return None;
            }
            if !sources.contains(&handoff.source_cell) {
                sources.push(handoff.source_cell.clone());
            }
        }
        let label = entry.label.trim();
        out.push(NewLiveAction {
            label: if label.is_empty() {
                fallback_label(prompt)
            } else {
                fallback_label(label)
            },
            prompt: prompt.to_string(),
            source_cells: sources,
        });
    }
    covered.iter().all(|c| *c).then_some(out)
}

/// Turn a review's handoffs into its live actions with one agent call. One
/// handoff needs no call. An unusable answer falls back to one action per
/// handoff.
pub fn deduplicate_handoffs(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    guardian: &GuardianView,
    cwd: &str,
    handoffs: &[CollectedHandoff],
    cancel: &CancelToken,
) -> Vec<NewLiveAction> {
    if handoffs.len() <= 1 {
        return undeduplicated(handoffs);
    }
    let gid = guardian.id.as_str();
    let resolved = match crate::guardian_merge::resolve_resolver_agent(
        guardian.resolver_agent.as_deref(),
        guardian.resolver_model.as_deref(),
        store,
        Path::new(cwd),
    ) {
        Ok(resolved) => resolved,
        Err(message) => {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {gid} live-action dedupe skipped (agent: {message}); keeping one action per handoff"
            );
            return undeduplicated(handoffs);
        }
    };
    let numbered = handoffs
        .iter()
        .enumerate()
        .map(|(i, h)| format!("Description {}:\n{}", i + 1, h.text))
        .collect::<Vec<_>>()
        .join("\n\n");
    let ws = Workspace::for_guardian(store, gid, cwd);
    let mut spec = RunnerSpec {
        squad_id: live_run_squad_id(gid),
        task: LIVE_ACTION_TASK.to_string(),
        cell_id: format!("live-dedupe-{}", crate::store::now_ms()),
        cwd: cwd.to_string(),
        prompt: Some(format!(
            "Merge these live-test descriptions for review {}.\n\n{numbered}",
            guardian.name
        )),
        command: None,
        shell: None,
        agent: resolved.backend.clone(),
        executable: resolved.executable.clone(),
        model: resolved.model.clone(),
        system_prompt: Some(DEDUPE_SYSTEM_PROMPT.to_string()),
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        effort: None,
        temperature: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: resolved.env.clone(),
        machine: ws.machine().map(str::to_string),
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
        waypoint_context: false,
        retry_attempt: 0,
        retry_after_unknown_default_seconds: crate::config::resolve(Path::new(cwd))
            .retry_after_unknown_default_seconds(),
        maximum_timeout: None,
        pass_score: None,
    };
    // Recorded before the agent starts, so the board can follow the
    // generation live -- the same way it follows auto-action generation.
    let _ = store
        .lock()
        .set_live_actions_generation_cell(gid, &spec.cell_id);
    let result = crate::guardian_merge::run_agent_with_stall_recovery(
        store, &mut spec, runner, cancel, None,
    );
    let _ = crate::guardian_merge::record_guardian_call_cost(
        store,
        gid,
        None,
        "live_action_dedupe",
        &result,
    );
    let parsed = (result.status == "done")
        .then(|| parse_dedupe_answer(&result.summary, handoffs))
        .flatten();
    let used_fallback = parsed.is_none();
    let actions = parsed.unwrap_or_else(|| undeduplicated(handoffs));
    note(
        store,
        gid,
        if used_fallback {
            crate::logging::LogLevel::WARNING
        } else {
            crate::logging::LogLevel::INFO
        },
        "live actions deduplicated",
        serde_json::json!({
            "cell_id": spec.cell_id,
            "handoffs": handoffs.len(),
            "actions": actions.len(),
            "fallback": used_fallback,
            "error": result.error,
        }),
    );
    actions
}

/// The live ref a run's commits are pinned on.
#[must_use]
pub fn live_ref_name(guardian_id: &str, run_id: &str) -> String {
    format!(
        "{LIVE_REF_PREFIX}/{}/{}",
        ref_safe(guardian_id),
        ref_safe(run_id)
    )
}

/// The cell id one live run's agent session (and its transcript) is keyed by.
#[must_use]
pub fn live_run_cell_id(action_index: usize, run_id: &str) -> String {
    format!("live-{action_index}-{}", ref_safe(run_id))
}

/// The squad namespace a review's agent sessions are keyed under.
#[must_use]
pub fn live_run_squad_id(guardian_id: &str) -> String {
    format!("guardian-{guardian_id}")
}

fn ref_safe(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// One live run to execute.
pub struct LiveRunRequest<'a> {
    pub guardian: &'a GuardianView,
    pub run_id: &'a str,
    pub action_index: usize,
    pub label: &'a str,
    pub prompt: &'a str,
    /// The checkout to run in (the action's prepared checkout).
    pub cwd: &'a str,
    /// Whether the run may commit fixes that will be carried back.
    pub allow_commits: bool,
}

/// How a live run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveRunEnd {
    /// The agent finished (whatever it found).
    Finished,
    /// The agent or its setup failed before producing a result.
    Failed,
    /// The run was cancelled.
    Cancelled,
    /// The review's spend cap was already reached; nothing ran.
    OverBudget,
}

impl LiveRunEnd {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::OverBudget => "over_budget",
        }
    }
}

/// What one live run produced.
#[derive(Debug, Clone)]
pub struct LiveRunOutcome {
    pub end: LiveRunEnd,
    /// The agent's final report.
    pub report: String,
    pub error: Option<String>,
    pub cost_usd: f64,
    /// The checkout's HEAD when the run started (T0).
    pub base_tip: Option<String>,
    /// Commits the run produced on top of `base_tip`, oldest first.
    pub commits: Vec<String>,
    /// The live ref the commits are pinned on, when there are any.
    pub live_ref: Option<String>,
    /// The cell id the agent's session and transcript are keyed by.
    pub cell_id: String,
    /// The exact prompt handed to the runner (`None` when the run never
    /// reached the runner).
    pub sent_prompt: Option<String>,
    /// The exact system prompt handed to the runner.
    pub sent_system_prompt: Option<String>,
    /// The agent's full final reply, before marker lines are dropped from
    /// `report`; the structured result line is parsed out of it.
    pub raw_reply: String,
}

fn outcome(end: LiveRunEnd, cell_id: &str, error: Option<String>) -> LiveRunOutcome {
    LiveRunOutcome {
        end,
        report: String::new(),
        error,
        cost_usd: 0.0,
        base_tip: None,
        commits: Vec::new(),
        live_ref: None,
        cell_id: cell_id.to_string(),
        sent_prompt: None,
        sent_system_prompt: None,
        raw_reply: String::new(),
    }
}

fn note(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    level: crate::logging::LogLevel,
    message: &'static str,
    payload: serde_json::Value,
) {
    let guard = store.lock();
    let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
        level,
        source: "guardian",
        message,
        scope: Some("live_action"),
        squad_id: None,
        guardian_id: Some(guardian_id),
        cell_id: payload.get("cell_id").and_then(|v| v.as_str()),
        task: Some(LIVE_ACTION_TASK),
        log_path: None,
        payload: payload.clone(),
        admin_only: false,
    });
}

/// Whether the review has already spent its `maximum_budget_usd` cap.
fn over_budget(store: &crate::store_lock::StoreHandle, guardian_id: &str) -> bool {
    let guard = store.lock();
    let Ok(Some(cap)) = guard.guardian_maximum_budget_usd(guardian_id) else {
        return false;
    };
    guard
        .guardian_cost_total(guardian_id)
        .is_ok_and(|(_, _, spent)| spent >= cap)
}

/// Execute one live run to completion. Blocks for the agent's whole session,
/// so callers run it on a worker thread. Never holds the store lock while the
/// agent runs.
pub fn execute_live_run(
    store: &crate::store_lock::StoreHandle,
    runner: &dyn Runner,
    req: &LiveRunRequest<'_>,
    cancel: &CancelToken,
) -> LiveRunOutcome {
    let gid = req.guardian.id.as_str();
    let cell_id = live_run_cell_id(req.action_index, req.run_id);
    if over_budget(store, gid) {
        note(
            store,
            gid,
            crate::logging::LogLevel::WARNING,
            "live run skipped: review budget reached",
            serde_json::json!({"run_id": req.run_id, "cell_id": cell_id}),
        );
        return outcome(
            LiveRunEnd::OverBudget,
            &cell_id,
            Some("the review's maximum_budget_usd cap is already reached".to_string()),
        );
    }

    let _hold = match crate::checkout_hold::hold(
        req.cwd,
        &format!("live-run:{gid}:{}", req.run_id),
        LIVE_RUN_HOLD_MAX_AGE,
    ) {
        Ok(hold) => Some(hold),
        Err(e) => {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {gid} live run {} could not record that it is using {}: {e}",
                req.run_id,
                req.cwd
            );
            None
        }
    };
    let ws = Workspace::for_guardian(store, gid, req.cwd);
    let base_tip = ws
        .git(&["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string());

    let resolved = match crate::guardian_merge::resolve_resolver_agent(
        req.guardian.resolver_agent.as_deref(),
        req.guardian.resolver_model.as_deref(),
        store,
        Path::new(req.cwd),
    ) {
        Ok(resolved) => resolved,
        Err(message) => {
            let mut failed = outcome(
                LiveRunEnd::Failed,
                &cell_id,
                Some(format!("unresolvable agent: {message}")),
            );
            failed.base_tip = base_tip;
            return failed;
        }
    };

    // The review's own environment first (what its actions run under), then
    // the agent profile's, which carries the agent's own credentials.
    let mut env = req.guardian.manual_checks_env.clone();
    env.extend(resolved.env.clone());
    let system_prompt = if req.allow_commits {
        LIVE_ACTION_COMMIT_SYSTEM_PROMPT
    } else {
        LIVE_ACTION_READ_ONLY_SYSTEM_PROMPT
    };
    let mut spec = RunnerSpec {
        squad_id: live_run_squad_id(gid),
        task: LIVE_ACTION_TASK.to_string(),
        cell_id: cell_id.clone(),
        cwd: req.cwd.to_string(),
        prompt: Some(format!("Live action: {}\n\n{}", req.label, req.prompt)),
        command: None,
        shell: None,
        agent: resolved.backend.clone(),
        executable: resolved.executable.clone(),
        model: resolved.model.clone(),
        system_prompt: Some(system_prompt.to_string()),
        system_prompt_position: None,
        timeout_sec: None,
        budget_tokens: None,
        maximum_budget_usd: None,
        maximum_context: None,
        auto_compact_threshold: None,
        maximum_tool_output_tokens: None,
        effort: None,
        temperature: None,
        proof: false,
        trace_context: None,
        resume_agent_session_id: None,
        assigned_agent_session_id: None,
        env_overrides: env,
        machine: ws.machine().map(str::to_string),
        tool_arg_truncate_chars: None,
        thrash_max_compactions: None,
        thrash_min_turn_gap: None,
        allow_personal_settings: false,
        allow_personal_memory: false,
        waypoint_context: false,
        retry_attempt: 0,
        retry_after_unknown_default_seconds: crate::config::resolve(Path::new(req.cwd))
            .retry_after_unknown_default_seconds(),
        maximum_timeout: None,
        pass_score: None,
    };

    note(
        store,
        gid,
        crate::logging::LogLevel::INFO,
        "live run starting",
        serde_json::json!({
            "run_id": req.run_id,
            "cell_id": cell_id,
            "action_index": req.action_index,
            "label": req.label,
            "cwd": req.cwd,
            "base_tip": base_tip,
            "allow_commits": req.allow_commits,
            "agent": resolved.backend,
            "model": resolved.model,
        }),
    );
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {gid} live run {} ({:?}) starting in {}",
        req.run_id,
        req.label,
        req.cwd
    );

    let sent_prompt = spec.prompt.clone();
    let sent_system_prompt = spec.system_prompt.clone();
    let result = crate::guardian_merge::run_agent_with_stall_recovery(
        store, &mut spec, runner, cancel, None,
    );
    let _ =
        crate::guardian_merge::record_guardian_call_cost(store, gid, None, "live_action", &result);

    let mut done = outcome(
        if cancel.is_cancelled() {
            LiveRunEnd::Cancelled
        } else if result.status == "done" {
            LiveRunEnd::Finished
        } else {
            LiveRunEnd::Failed
        },
        &cell_id,
        result.error.clone(),
    );
    done.report = clean_report(&result.summary);
    done.raw_reply.clone_from(&result.summary);
    done.sent_prompt = sent_prompt;
    done.sent_system_prompt = sent_system_prompt;
    done.cost_usd = result.cost_usd;
    done.base_tip.clone_from(&base_tip);

    if let Some(base) = base_tip.as_deref() {
        if req.allow_commits && done.end != LiveRunEnd::Cancelled {
            commit_leftover_edits(&ws, req.label);
        }
        done.commits = commits_since(&ws, base);
        if !done.commits.is_empty() {
            let live_ref = live_ref_name(gid, req.run_id);
            match ws.git(&["update-ref", &live_ref, "HEAD"]) {
                Ok(_) => done.live_ref = Some(live_ref),
                Err(e) => crate::rlog!(
                    ERROR,
                    "ralphus [guardian] review {gid} live run {} could not pin its commits on {live_ref}: {e}",
                    req.run_id
                ),
            }
        }
    }

    note(
        store,
        gid,
        if done.end == LiveRunEnd::Finished {
            crate::logging::LogLevel::INFO
        } else {
            crate::logging::LogLevel::WARNING
        },
        "live run finished",
        serde_json::json!({
            "run_id": req.run_id,
            "cell_id": cell_id,
            "end": done.end.as_str(),
            "error": done.error,
            "cost_usd": done.cost_usd,
            "base_tip": done.base_tip,
            "commits": done.commits,
            "live_ref": done.live_ref,
            "report": done.report.chars().take(REPORT_LOG_CHARS).collect::<String>(),
        }),
    );
    crate::rlog!(
        INFO,
        "ralphus [guardian] review {gid} live run {} ended {} with {} commit(s)",
        req.run_id,
        done.end.as_str(),
        done.commits.len()
    );
    done
}

/// The agent's report without ralphus's own reply markers: everything from a
/// `RALPHUS_GHOST:` handoff on, and any other `RALPHUS_` marker line, is
/// bookkeeping for ralphus, not part of what the live test found.
#[must_use]
pub fn clean_report(reply: &str) -> String {
    let mut kept = Vec::new();
    for line in reply.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("RALPHUS_GHOST:") {
            break;
        }
        if trimmed.starts_with("RALPHUS_") {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n").trim().to_string()
}

/// Commit tracked-file edits the agent left uncommitted. Untracked files are
/// left alone: the prepared checkout holds build output, which must never be
/// committed into a review.
fn commit_leftover_edits(ws: &Workspace, label: &str) {
    if ws.git(&["add", "--update"]).is_err() {
        return;
    }
    // `diff --cached --quiet` exits non-zero exactly when something is staged.
    if ws.git(&["diff", "--cached", "--quiet"]).is_ok() {
        return;
    }
    let _ = ws.git(&[
        "commit",
        "--message",
        &format!("Uncommitted changes left by live action \"{label}\""),
    ]);
}

/// Commits on HEAD that are not on `base`, oldest first.
fn commits_since(ws: &Workspace, base: &str) -> Vec<String> {
    ws.git(&["rev-list", "--reverse", &format!("{base}..HEAD")])
        .map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A runner standing in for the agent: runs `act` in the spec's cwd and
    /// records the spec it was given.
    struct ScriptedAgent {
        act: Box<dyn Fn(&Path) + Send + Sync>,
        reply: String,
        seen: parking_lot::Mutex<Vec<RunnerSpec>>,
    }

    impl Runner for ScriptedAgent {
        fn run(&self, spec: &RunnerSpec) -> crate::runner::RunnerResult {
            (self.act)(Path::new(&spec.cwd));
            self.seen.lock().push(spec.clone());
            let mut result = crate::runner::RunnerResult::failure("");
            result.status = "done".to_string();
            result.error = None;
            result.summary.clone_from(&self.reply);
            result.cost_usd = 0.25;
            result
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-live-run-{tag}-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        // Repo-local identity: the daemon commits leftover edits with plain git,
        // and a CI runner has no global one.
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        std::fs::write(dir.join("app.txt"), "v1\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    fn setup(
        tag: &str,
    ) -> (
        Arc<crate::store_lock::StoreMutex>,
        GuardianView,
        std::path::PathBuf,
    ) {
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let dir = repo(tag);
        let id = store
            .lock()
            .create_guardian("live", "main", &dir.to_string_lossy())
            .unwrap();
        let g = store.lock().get_guardian(&id).unwrap();
        (store, g, dir)
    }

    fn request<'a>(g: &'a GuardianView, cwd: &'a str, allow_commits: bool) -> LiveRunRequest<'a> {
        LiveRunRequest {
            guardian: g,
            run_id: "r1",
            action_index: 0,
            label: "Smoke test",
            prompt: "Start the app and check it greets.",
            cwd,
            allow_commits,
        }
    }

    #[test]
    fn a_live_run_runs_the_prompt_as_a_review_agent_and_records_its_report() {
        let (store, g, dir) = setup("report");
        let agent = ScriptedAgent {
            act: Box::new(|_| {}),
            reply: "Tested the greeting: it works.".to_string(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let cwd = dir.to_string_lossy().to_string();
        let out = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, true),
            &CancelToken::never(),
        );

        assert_eq!(out.end, LiveRunEnd::Finished);
        assert_eq!(out.report, "Tested the greeting: it works.");
        assert!(out.commits.is_empty());
        assert!(out.live_ref.is_none());
        let seen = agent.seen.lock();
        let spec = &seen[0];
        assert_eq!(spec.squad_id, format!("guardian-{}", g.id));
        assert_eq!(spec.task, LIVE_ACTION_TASK);
        assert_eq!(spec.cell_id, "live-0-r1");
        assert!(spec.prompt.as_deref().unwrap().contains("Start the app"));
        assert_eq!(
            spec.system_prompt.as_deref(),
            Some(LIVE_ACTION_COMMIT_SYSTEM_PROMPT)
        );
        let (_, _, spent) = store.lock().guardian_cost_total(&g.id).unwrap();
        assert!(
            (spent - 0.25).abs() < 1e-9,
            "live run cost is charged to the review"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_runs_commits_and_leftover_edits_are_pinned_on_its_live_ref() {
        let (store, g, dir) = setup("commits");
        let agent = ScriptedAgent {
            act: Box::new(|cwd| {
                std::fs::write(cwd.join("app.txt"), "v2\n").unwrap();
                git(cwd, &["commit", "-qam", "Fix greeting found while testing"]);
                // A second, uncommitted edit plus untracked build output.
                std::fs::write(cwd.join("app.txt"), "v3\n").unwrap();
                std::fs::write(cwd.join("build.log"), "noise").unwrap();
            }),
            reply: "Fixed it.".to_string(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let cwd = dir.to_string_lossy().to_string();
        let base = git(&dir, &["rev-parse", "HEAD"]);
        let out = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, true),
            &CancelToken::never(),
        );

        assert_eq!(out.base_tip.as_deref(), Some(base.as_str()));
        assert_eq!(
            out.commits.len(),
            2,
            "the agent's commit plus its leftover edit"
        );
        let live_ref = out.live_ref.clone().unwrap();
        assert_eq!(live_ref, live_ref_name(&g.id, "r1"));
        assert_eq!(
            git(&dir, &["rev-parse", &live_ref]),
            *out.commits.last().unwrap()
        );
        let tracked = git(&dir, &["ls-tree", "-r", "--name-only", &live_ref]);
        assert!(
            !tracked.contains("build.log"),
            "untracked output is never committed"
        );
        assert_eq!(git(&dir, &["show", &format!("{live_ref}:app.txt")]), "v3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_only_run_gets_the_read_only_prompt_and_commits_nothing_itself() {
        let (store, g, dir) = setup("read-only");
        let agent = ScriptedAgent {
            act: Box::new(|cwd| std::fs::write(cwd.join("app.txt"), "edited\n").unwrap()),
            reply: "Broken: it crashes on start.".to_string(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let cwd = dir.to_string_lossy().to_string();
        let out = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, false),
            &CancelToken::never(),
        );
        assert!(
            out.commits.is_empty(),
            "a read-only run's edits are never committed"
        );
        assert_eq!(
            agent.seen.lock()[0].system_prompt.as_deref(),
            Some(LIVE_ACTION_READ_ONLY_SYSTEM_PROMPT)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_run_holds_its_checkout_while_the_agent_runs() {
        let (store, g, dir) = setup("hold");
        let cwd = dir.to_string_lossy().to_string();
        let probe = cwd.clone();
        let held_during = Arc::new(parking_lot::Mutex::new(None));
        let held_flag = Arc::clone(&held_during);
        let agent = ScriptedAgent {
            act: Box::new(move |_| {
                *held_flag.lock() = Some(crate::checkout_hold::is_held(&probe));
            }),
            reply: String::new(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let _ = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, true),
            &CancelToken::never(),
        );
        assert_eq!(*held_during.lock(), Some(true));
        assert!(
            !crate::checkout_hold::is_held(&cwd),
            "released when the run ends"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A squad with one task holding two prompt cells.
    pub(crate) fn insert_squad(store: &mut crate::store::Store) -> String {
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            r#"
[[task]]
name = "build"
[[task.cell]]
id = "one"
cwd = "/repo"
prompt = "first"
[[task.cell]]
id = "two"
cwd = "/repo"
prompt = "second"
"#,
        )
        .unwrap();
        store.insert_squad(&file, None, false).unwrap()
    }

    #[test]
    fn a_report_drops_ralphus_markers() {
        let reply = "Tested it: /greet 404s.\nRALPHUS_PROPHECY: deferred: wire it\nStill broken.\n\
                     RALPHUS_GHOST:\n- routes missing";
        assert_eq!(
            clean_report(reply),
            "Tested it: /greet 404s.\nStill broken."
        );
    }

    #[test]
    fn parses_no_marker_as_no_marker() {
        assert_eq!(parse_live_handoffs("all done"), LiveHandoffs::NoMarker);
    }

    #[test]
    fn parses_several_multi_line_handoffs_and_stops_at_the_ghost() {
        let reply = "Implemented it.\n\
            RALPHUS_LIVE_ACTION: Start the board with `scripts/dev.sh`.\n\
            Open Reviews and expand a branch; the rate-limit row shows a countdown.\n\
            RALPHUS_LIVE_ACTION: Run `ralphus review show r1` and check the fields print.\n\
            RALPHUS_GHOST: - nothing odd";
        assert_eq!(
            parse_live_handoffs(reply),
            LiveHandoffs::Tests(vec![
                "Start the board with `scripts/dev.sh`.\nOpen Reviews and expand a branch; the rate-limit row shows a countdown."
                    .to_string(),
                "Run `ralphus review show r1` and check the fields print.".to_string(),
            ])
        );
    }

    #[test]
    fn a_prophecy_line_ends_a_handoff() {
        let reply = "RALPHUS_LIVE_ACTION: run it\nRALPHUS_PROPHECY: hazard: x\ntrailing prose";
        assert_eq!(
            parse_live_handoffs(reply),
            LiveHandoffs::Tests(vec!["run it".to_string()])
        );
    }

    #[test]
    fn parses_the_explicit_nothing_to_test_answer() {
        assert_eq!(
            parse_live_handoffs("RALPHUS_LIVE_ACTION: (Nothing to test live)\nRALPHUS_GHOST: -"),
            LiveHandoffs::NothingToTest
        );
        // A real test next to the "nothing" answer wins.
        assert_eq!(
            parse_live_handoffs(
                "RALPHUS_LIVE_ACTION: (nothing to test live)\nRALPHUS_LIVE_ACTION: run it"
            ),
            LiveHandoffs::Tests(vec!["run it".to_string()])
        );
    }

    #[test]
    fn handoffs_round_trip_and_a_new_reply_replaces_the_old_one() {
        let mut store = crate::store::Store::open_in_memory().unwrap();
        let squad = insert_squad(&mut store);
        store
            .replace_cell_live_handoffs(
                &squad,
                0,
                1,
                &LiveHandoffs::Tests(vec!["a".to_string(), "b".to_string()]),
            )
            .unwrap();
        let got = store.cell_live_handoffs(&squad, 0, 1).unwrap();
        assert_eq!(got.tests, vec!["a".to_string(), "b".to_string()]);
        assert!(!got.nothing_to_test);
        assert!(got.recorded_at_ms.is_some());

        store
            .replace_cell_live_handoffs(&squad, 0, 1, &LiveHandoffs::NothingToTest)
            .unwrap();
        let got = store.cell_live_handoffs(&squad, 0, 1).unwrap();
        assert!(got.tests.is_empty());
        assert!(got.nothing_to_test);

        store
            .replace_cell_live_handoffs(&squad, 0, 1, &LiveHandoffs::NoMarker)
            .unwrap();
        assert_eq!(
            store.cell_live_handoffs(&squad, 0, 1).unwrap(),
            CellLiveHandoffs::default()
        );
    }

    fn handoff(cell: &str, text: &str) -> CollectedHandoff {
        CollectedHandoff {
            source_cell: cell.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn a_dedupe_answer_merges_and_keeps_every_source() {
        let handoffs = vec![
            handoff("c1", "start the board; open reviews"),
            handoff("c2", "start the board; expand a branch"),
            handoff("c3", "run ralphus review show"),
        ];
        let reply = r#"Here you go: {"live_actions": [
            {"label": "Board review inspector", "prompt": "start the board; open reviews; expand a branch", "sources": [1, 2]},
            {"label": "CLI review show", "prompt": "run ralphus review show", "sources": [3]}
        ]}"#;
        let actions = parse_dedupe_answer(reply, &handoffs).unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0].source_cells,
            vec!["c1".to_string(), "c2".to_string()]
        );
        assert_eq!(actions[1].label, "CLI review show");
    }

    #[test]
    fn an_unusable_dedupe_answer_is_rejected() {
        let handoffs = vec![handoff("c1", "a"), handoff("c2", "b")];
        for reply in [
            "not json",
            r#"{"live_actions": []}"#,
            // Description 2 dropped.
            r#"{"live_actions": [{"label": "x", "prompt": "a", "sources": [1]}]}"#,
            // Out of range.
            r#"{"live_actions": [{"label": "x", "prompt": "a", "sources": [1, 2, 3]}]}"#,
            // Zero is not a description number.
            r#"{"live_actions": [{"label": "x", "prompt": "a", "sources": [0, 1, 2]}]}"#,
            // Description 1 used twice in one entry.
            r#"{"live_actions": [{"label": "x", "prompt": "a", "sources": [1, 1, 2]}]}"#,
            // Description 1 used in two entries.
            r#"{"live_actions": [{"label": "x", "prompt": "a", "sources": [1, 2]}, {"label": "y", "prompt": "b", "sources": [1]}]}"#,
            // Empty prompt and empty source list.
            r#"{"live_actions": [{"label": "x", "prompt": " ", "sources": [1, 2]}]}"#,
            r#"{"live_actions": [{"label": "x", "prompt": "a", "sources": []}, {"label": "y", "prompt": "b", "sources": [1, 2]}]}"#,
        ] {
            assert!(parse_dedupe_answer(reply, &handoffs).is_none(), "{reply}");
        }
    }

    #[test]
    fn a_single_handoff_needs_no_agent() {
        let (store, g, dir) = setup("single");
        let agent = ScriptedAgent {
            act: Box::new(|_| panic!("one handoff needs no dedupe call")),
            reply: String::new(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let actions = deduplicate_handoffs(
            &store,
            &agent,
            &g,
            &dir.to_string_lossy(),
            &[handoff("c1", "Open the board.\nCheck the row.")],
            &CancelToken::never(),
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].label, "Open the board.");
        assert_eq!(actions[0].prompt, "Open the board.\nCheck the row.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_agent_that_answers_badly_keeps_one_action_per_handoff() {
        let (store, g, dir) = setup("bad-dedupe");
        let agent = ScriptedAgent {
            act: Box::new(|_| {}),
            reply: "I merged them all, trust me".to_string(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let actions = deduplicate_handoffs(
            &store,
            &agent,
            &g,
            &dir.to_string_lossy(),
            &[handoff("c1", "a"), handoff("c2", "b")],
            &CancelToken::never(),
        );
        assert_eq!(actions.len(), 2, "no test is silently dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_latest_finished_cell_of_each_task_seeds_the_review() {
        let mut store = crate::store::Store::open_in_memory().unwrap();
        let squad = insert_squad(&mut store);
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        for (idx, finished) in [(0_i64, 100_i64), (1, 200)] {
            store
                .conn
                .execute(
                    "UPDATE cells SET state='done', finished_at_ms=?, review_guardian_id=?
                     WHERE squad_id=? AND task_idx=0 AND idx=?",
                    rusqlite::params![finished, gid, squad, idx],
                )
                .unwrap();
        }
        store
            .replace_cell_live_handoffs(
                &squad,
                0,
                0,
                &LiveHandoffs::Tests(vec!["phase one".into()]),
            )
            .unwrap();
        store
            .replace_cell_live_handoffs(
                &squad,
                0,
                1,
                &LiveHandoffs::Tests(vec!["phase two".into()]),
            )
            .unwrap();

        let collected = collect_review_handoffs_conn(&store.conn, &gid).unwrap();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].text, "phase two");
        assert_eq!(
            collected[0].source_cell,
            crate::ghost::cell_uri(&squad, 0, 1)
        );

        // A still-running cell contributes nothing yet.
        store
            .conn
            .execute(
                "UPDATE cells SET state='running' WHERE squad_id=? AND idx=1",
                rusqlite::params![squad],
            )
            .unwrap();
        let collected = collect_review_handoffs_conn(&store.conn, &gid).unwrap();
        assert_eq!(collected[0].text, "phase one");
    }

    #[test]
    fn a_reviews_live_actions_are_written_once() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let first = vec![NewLiveAction {
            label: "a".into(),
            prompt: "do a".into(),
            source_cells: vec!["c1".into()],
        }];
        assert!(store.insert_guardian_live_actions(&gid, &first).unwrap());
        assert!(
            !store
                .insert_guardian_live_actions(
                    &gid,
                    &[NewLiveAction {
                        label: "b".into(),
                        prompt: "do b".into(),
                        source_cells: vec![],
                    }]
                )
                .unwrap(),
            "the set is fixed once written"
        );
        let stored = store.guardian_live_actions(&gid).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].prompt, "do a");
        assert_eq!(stored[0].source_cells, vec!["c1".to_string()]);
    }

    #[test]
    fn a_review_over_its_budget_runs_nothing() {
        let (store, g, dir) = setup("budget");
        store
            .lock()
            .set_guardian_maximum_budget_usd(&g.id, Some(0.0))
            .unwrap();
        let agent = ScriptedAgent {
            act: Box::new(|_| panic!("must not run")),
            reply: String::new(),
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        let cwd = dir.to_string_lossy().to_string();
        let out = execute_live_run(
            &store,
            &agent,
            &request(&g, &cwd, true),
            &CancelToken::never(),
        );
        assert_eq!(out.end, LiveRunEnd::OverBudget);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
