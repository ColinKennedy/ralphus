//! Invoking the cell runner.
//!
//! The daemon builds a [`RunnerSpec`] for each cell, hands it to a [`Runner`],
//! and gets back a [`RunnerResult`]. The real implementation
//! ([`SubprocessRunner`]) spawns the `ralphus-runner` process and speaks the JSON
//! contract in `runner/src/spec.rs`. The trait keeps the scheduler
//! testable with an in-process fake.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::procreg::ProcRegistry;
use crate::store::{CellRow, NodeState, Store};
use crate::tmux::Tmux;

/// Prefix the runner subprocess writes to stderr before a JSON-encoded
/// [`RunnerEvent`], so its structured events reach Cartographer without
/// touching stdout (reserved for the `CellSpec`/`CellResult` contract).
/// Mirrors the existing `RALPHUS_PROOF: PASS/FAIL` marker-parsing pattern.
pub const EVENT_MARKER: &str = "RALPHUS_EVENT: ";

/// The message every agent backend gives its per-turn token/cost snapshot
/// (RAL-161), mirrored from `runner/src/cartographer.rs::LIVE_USAGE_MESSAGE`.
/// [`forward_runner_event`] folds such an event's payload onto the cell row
/// and hands it to the live cost-cap check, then drops it rather than
/// persisting a Cartographer row: the agent emits one per assistant turn, so
/// keeping them buried a squad's timeline (`ralphus squad timeline`) under
/// hundreds of `INFO claude-code live usage` lines carrying nothing the
/// cell's own `tokens_in`/`tokens_out`/`cost_usd` columns don't already hold.
const LIVE_USAGE_MESSAGE: &str = "live usage";

/// One structured event forwarded from the runner subprocess over the
/// `RALPHUS_EVENT:` stderr marker (RAL-98). `squad_id`/`cell_id`/`task` fall
/// back to the owning [`RunnerSpec`] when the event itself omits them.
#[derive(Debug, Deserialize)]
struct RunnerEvent {
    source: String,
    message: String,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    squad_id: Option<String>,
    #[serde(default)]
    cell_id: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
}

/// Live token/cost usage extracted from a runner event's payload (RAL-161),
/// mirroring the `agent_session_id` capture alongside it. Also the shape
/// [`Store::set_cell_live_usage`] persists and the one every
/// snapshot-derived [`RunnerResult`] constructor
/// ([`RunnerResult::cost_exceeded`], [`RunnerResult::detached`]) takes, so
/// growing the set of tracked usage fields is a one-place edit.
///
/// `pub` (not private): [`crate::remote_runner::ProviderRunner`] needs this
/// too, to drive the same live cost-cap kill for a remote session that
/// [`SubprocessRunner`] already does locally.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LiveUsage {
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// RAL-326: prompt-cache write/read tokens, carried alongside (never
    /// folded into) `tokens_in`.
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

/// What [`forward_runner_event`] learned from one event, for the caller's own
/// use beyond persisting it to Cartographer: a freshly-captured agent
/// session id (for tmux auto-reattach) and/or a fresh live-usage snapshot
/// (for the cost-cap kill check).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ForwardedEvent {
    pub(crate) agent_session_id: Option<String>,
    pub(crate) live_usage: Option<LiveUsage>,
}

/// The JSON spec sent to the runner on stdin (mirrors the runner's `CellSpec`).
#[derive(Debug, Clone, Serialize)]
pub struct RunnerSpec {
    /// Owning squad id.
    pub squad_id: String,
    /// Owning task name.
    pub task: String,
    /// Cell id.
    pub cell_id: String,
    /// Working directory.
    pub cwd: String,
    /// AI prompt, if this is a prompt cell.
    pub prompt: Option<String>,
    /// Shell command, if this is a command cell.
    pub command: Option<String>,
    /// Agent program.
    pub agent: String,
    /// Optional executable override for subprocess-spawning backends
    /// (`claude-code`, `codex`, `raw`). Never set for native API backends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Model, if any.
    pub model: Option<String>,
    /// System-prompt text to deliver as an *appended* system prompt (via the
    /// backend's own flag, e.g. Claude Code's `--append-system-prompt`) rather
    /// than concatenated into `prompt`. `None` when unset (RAL-5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Placement of `system_prompt` — only `"append"` today. `None` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_position: Option<String>,
    /// Wall-clock timeout in seconds; the daemon kills the runner subprocess if
    /// it exceeds this. `None` means no limit (RAL-15).
    pub timeout_sec: Option<u64>,
    /// Total-token budget; the runner fails the cell if usage exceeds it.
    /// `None` means no cap (RAL-15).
    pub budget_tokens: Option<u64>,
    /// USD spend cap; the daemon kills the runner subprocess mid-run and
    /// fails the cell once its live `cost_usd` exceeds this. `None` means
    /// no cap (RAL-161). Purely a daemon-side enforcement signal — not
    /// consumed by the runner itself, so it's serialized here for
    /// completeness but harmlessly ignored by `CellSpec::from_json`.
    pub maximum_budget_usd: Option<f64>,
    /// Context-window token limit (RAL-304), delivered to the backend via
    /// its own mechanism (env var/CLI arg/settings file -- see
    /// `ralphus_core::schema::agent_supports_maximum_context`). `None` means
    /// no cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_context: Option<u64>,
    /// Auto-compact trigger threshold in tokens (RAL-304), delivered to the
    /// backend via its own mechanism -- see
    /// `ralphus_core::schema::agent_supports_auto_compact_threshold`.
    /// Accepted by a wider set of backends than [`Self::maximum_context`].
    /// `None` means no explicit threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold: Option<u64>,
    /// Tool-output token cap (RAL-333), delivered to the backend via its own
    /// mechanism (env var/CLI arg/settings file -- see
    /// `ralphus_core::schema::agent_supports_maximum_tool_output_tokens`). `None`
    /// means no cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_tool_output_tokens: Option<u64>,
    /// True when this spec is an `agent`-kind proof step rather than a
    /// normal cell: the runner wraps `prompt` with verdict-reporting
    /// instructions and returns a `proofed` result instead of just "ran".
    pub proof: bool,
    /// W3C `traceparent` of the OpenTelemetry span this cell/proof run is
    /// a child of (RAL-96), so the runner's own spans continue the
    /// same trace instead of starting a disconnected one. `None` when tracing
    /// is not configured (see `daemon/src/otel.rs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<String>,
    /// When set, tells the runner to resume an existing agent conversation
    /// (`claude -p --resume <id>` / `codex exec resume <id>`) instead of
    /// starting a fresh one. Two producers set it today: the claude-code
    /// backend's own still-working async retry loop, and (RAL-248) the
    /// scheduler when a cell continues from a completed dependency's session
    /// (cross-cell session sharing) — see `scheduler::resolve_shared_session_id`
    /// and its model-mismatch guard. The daemon's [`SubprocessRunner::run_via_tmux`]
    /// auto-reattach retry also sets it when a cell's tmux pane vanishes
    /// mid-run but its live `agent_session_id` was already captured. `None`
    /// for a normal (first attempt) invocation. Both the claude-code and
    /// codex backends honor it; the hand-rolled backends accept and ignore it
    /// (mirrors `system_prompt`'s precedent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_agent_session_id: Option<String>,
    /// A session id the daemon generated and persisted to the cell's row
    /// *before* the runner subprocess was spawned (RAL-288 Stage 1), so the
    /// claude-code backend can pass `--session-id <id>` at launch instead of
    /// waiting on Claude's own `system/init` event to learn it. Closes the
    /// window where "Open Agent" is disabled for the very start of a run.
    /// Mutually exclusive with `resume_agent_session_id` in practice (the
    /// scheduler only generates one when that field is `None`); the
    /// claude-code backend prefers `--resume` when both happen to be set.
    /// `None` for anything that isn't a fresh claude-code cell dispatch —
    /// proof steps, Guardian resolver invocations, and non-claude-code
    /// backends (which have no equivalent pre-assignment flag and ignore
    /// this field, mirroring `resume_agent_session_id`'s precedent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_agent_session_id: Option<String>,
    /// Persistent, user-set environment-variable overrides for the owning
    /// squad (RAL-150), applied to the spawned `ralphus-runner` subprocess's
    /// own environment — never sent over the stdin wire contract itself (the
    /// runner needs nothing about them beyond inheriting them from its own
    /// process environment, same as any other env var). See
    /// [`SubprocessRunner::run_via_tmux_attempt`]
    /// ([`crate::tmux::build_command_line_with_env`]) for where this actually
    /// takes effect. Empty for every caller except the scheduler's own
    /// cell/proof dispatch, which fills it in from
    /// [`crate::store::Store::get_squad_env_overrides`].
    #[serde(skip)]
    pub env_overrides: BTreeMap<String, String>,
    /// The resolved `machine` this cell runs on (RAL-185), as authored —
    /// e.g. `"incredibuild:A"`. `None` (the common case) means the daemon's
    /// own host, and the spec is handled by [`SubprocessRunner`] exactly as
    /// before.
    ///
    /// This is daemon-side routing metadata consumed by
    /// [`crate::remote_runner::MachineRouter`]; it is serialized so a provider
    /// can see which of its machines a spec was destined for, and the runner
    /// ignores it (`CellSpec::from_json` reads named fields only, so
    /// an extra key is inert).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// RAL-303: the resolved `[live_view] tool_arg_truncate_chars` value
    /// (`crate::config::LiveViewConfig::tool_arg_truncate_chars`), forwarded
    /// over the stdin wire contract so the claude-code backend's
    /// `format_tool_input` knows how much of a `tool_use` argument to render
    /// into the Live View tmux pane before truncating. Resolved once here
    /// (daemon-side, the sole owner of `.ralphus.toml`) rather than read by
    /// the runner subprocess itself -- the runner has no per-cell cwd config
    /// resolution of its own. `None` means the runner falls back to its own
    /// default (200).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_arg_truncate_chars: Option<u32>,
    /// RAL-339: the resolved `.ralphus.toml` `[thrash]` thresholds -- N
    /// (compact count, `crate::config::ThrashConfig::max_compactions`) and M
    /// (turn-gap, `crate::config::ThrashConfig::min_turn_gap`), forwarded
    /// over the stdin wire contract the same way `tool_arg_truncate_chars`
    /// is, so the runner's shared `ThrashTracker` (`runner/src/thrash.rs`)
    /// knows when a run's compaction cadence counts as thrashing. Resolved
    /// once here (daemon-side, the sole owner of `.ralphus.toml`) rather
    /// than read by the runner subprocess itself. `None` means the runner
    /// falls back to its own defaults (3 / 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thrash_max_compactions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thrash_min_turn_gap: Option<u32>,
    /// RAL-336: the resolved `[agent_isolation] allow_personal_settings`
    /// value (`crate::config::AgentIsolationConfig::allow_personal_settings`),
    /// forwarded over the stdin wire contract so the spawned backend knows
    /// whether it may read the operator's real personal settings (Claude Code
    /// `--setting-sources`/`settings.json`, Codex `config.toml`, Pi
    /// `settings.json`/`models.json`) instead of an isolated copy. Always
    /// serialized (no `skip_serializing_if`) so the wire contract has no
    /// ambiguous "unset" state -- defaults to `false` (isolated) wherever this
    /// spec isn't resolved from `.ralphus.toml` (proof/internal invocations).
    pub allow_personal_settings: bool,
    /// RAL-336: the resolved `[agent_isolation] allow_personal_memory` value
    /// (`crate::config::AgentIsolationConfig::allow_personal_memory`) --
    /// same rationale as [`Self::allow_personal_settings`], but for the
    /// operator's personal memory (Claude Code's global `CLAUDE.md`/history,
    /// Codex/Pi's equivalent). Defaults to `false` (isolated).
    pub allow_personal_memory: bool,
}

/// Generate a fresh RFC 4122 version-4 (random) UUID, formatted as the
/// standard 8-4-4-4-12 hex string Claude Code's `--session-id` expects
/// (RAL-288 Stage 1). Uses `getrandom` directly rather than pulling in the
/// `uuid` crate for one id format — mirrors `token.rs::generate`'s precedent
/// for the daemon's other from-scratch random-id need.
pub(crate) fn generate_agent_session_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS randomness source available");
    // RFC 4122 §4.4: stamp the version (4) and variant (10) bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-\
         {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

// Keep these strings in sync with `runner/src/execute.rs`, which
// appends them right before invoking the backend. The board shows the effective
// read-only system prompt the agent actually received, not just the user-authored
// cell config fragment.
//
// The assembled prompt (caller-authored fragment first, then these in the
// order the `combine_system_prompts` calls below list them) is organized as
// `## Background` -> `## Regarding Tools` -> `## Conclusion`: the
// non-interactive and tools fragments open their own sections, and the
// async fragment opens `## Conclusion`, so the proof/ghost fragment that
// follows it lands inside that section.
const PROOF_SYSTEM_PROMPT: &str = "This is a PROOF step, not a normal task. Investigate whether the \
     task holds, attempting to fix any problems you find so the check passes \
     if you can reasonably do so. When you are done, your FINAL line of \
     output must be exactly one of:\nRALPHUS_PROOF: PASS\nRALPHUS_PROOF: FAIL\nwith \
     nothing else on that line.";
const GHOST_SYSTEM_PROMPT: &str = "Operational logging note, not a request to change your behavior: this \
     ralphus task run keeps a short handoff record for whichever agent picks \
     up dependent work next. That agent will see your code changes but not \
     this conversation. After you finish the task above, add one final \
     section to your reply, starting on its own line with the exact marker \
     'RALPHUS_GHOST:', followed by up to 5 short bullet points. Only include \
     things a future agent could NOT already learn by reading `git log` or \
     the diff: places you struggled, workarounds you used, issues you noticed \
     but did not fix, and open questions. This is not a changelog. If there \
     is truly nothing worth flagging, write 'RALPHUS_GHOST: (nothing to \
     report)'.";
const ASYNC_SYSTEM_PROMPT: &str = "## Conclusion\nThis is a single, non-interactive invocation — no \
     human will check back on you or answer follow-up questions, though \
     Ralphus may re-invoke you synchronously to continue. Never use an \
     asynchronous/background/'notify me later' tool for anything this session \
     depends on, and never launch the thing you are checking as a \
     background/detached process and end your turn while it is still running; \
     those require a persistent session this invocation does not have. If a \
     check genuinely takes a long time, block and wait for it synchronously \
     in the foreground within this same turn — it is fine for that to take a \
     long time. If, despite that, you truly cannot reach a definitive result \
     before you must stop, end your reply with 'RALPHUS_STILL_WORKING: \
     <one-line reason>' as the last line instead of trailing off — you will \
     be re-invoked shortly to continue synchronously from where you left \
     off, though only a bounded number of times, so prefer just finishing \
     the check yourself.";
const TOOLS_SYSTEM_PROMPT: &str = "## Regarding Tools\nPrefer `rg` for shell searches; \
     use `grep` only when `rg` is unavailable or you need grep-specific \
     behavior. In shell examples, use `rg \"pattern\" .`.";
const NON_INTERACTIVE_SYSTEM_PROMPT: &str = "## Background\nYou are running unattended in a non-interactive \
     cell — no human is available to answer questions or approve a plan. \
     Never ask a clarifying question, never stop to present a plan for \
     confirmation, and never pause waiting for input. Make the most \
     reasonable judgment call yourself and continue until the task is \
     complete.";

fn combine_system_prompts<'a>(parts: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    let combined = parts.into_iter().flatten().collect::<Vec<_>>().join("\n\n");
    if combined.is_empty() {
        None
    } else {
        Some(combined)
    }
}

fn subproject_system_prompt_addendum(subprojects: &[String]) -> Option<String> {
    if subprojects.is_empty() {
        return None;
    }
    let list = subprojects
        .iter()
        .map(|p| format!("`{p}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "This repository is a monorepo. Your task is scoped to the \
         following package(s): {list}. Focus your edits on those \
         subdirectories; the entire repository is accessible but your \
         changes must stay within those paths unless the task \
         explicitly requires otherwise."
    ))
}

pub(crate) fn cell_config_system_prompt(
    stored_system_prompt: Option<&str>,
    subprojects: &[String],
) -> Option<String> {
    let addendum = subproject_system_prompt_addendum(subprojects);
    combine_system_prompts([stored_system_prompt, addendum.as_deref()])
}

pub(crate) fn effective_cell_system_prompt(
    stored_system_prompt: Option<&str>,
    subprojects: &[String],
) -> String {
    let base = cell_config_system_prompt(stored_system_prompt, subprojects);
    combine_system_prompts([
        base.as_deref(),
        Some(NON_INTERACTIVE_SYSTEM_PROMPT),
        Some(TOOLS_SYSTEM_PROMPT),
        Some(ASYNC_SYSTEM_PROMPT),
        Some(GHOST_SYSTEM_PROMPT),
    ])
    .expect("cell prompts always include ralphus system instructions")
}

pub(crate) fn effective_proof_system_prompt(spec_system_prompt: Option<&str>) -> String {
    combine_system_prompts([
        spec_system_prompt,
        Some(NON_INTERACTIVE_SYSTEM_PROMPT),
        Some(TOOLS_SYSTEM_PROMPT),
        Some(ASYNC_SYSTEM_PROMPT),
        Some(PROOF_SYSTEM_PROMPT),
    ])
    .expect("proof prompts always include ralphus system instructions")
}

/// RAL-303: the effective `[live_view] tool_arg_truncate_chars`
/// (global-under-project, current-dir-based -- same convention
/// `crate::config::load_live_view_config` itself documents), read fresh at
/// spec-construction time so a config change takes effect on a squad's next
/// cell without a daemon restart.
fn resolved_tool_arg_truncate_chars() -> u32 {
    crate::config::load_live_view_config().tool_arg_truncate_chars()
}

/// RAL-339: the effective `.ralphus.toml` `[thrash]` thresholds (N, M), read
/// fresh at spec-construction time -- mirrors
/// [`resolved_tool_arg_truncate_chars`]'s "read live so a config change takes
/// effect on a squad's next cell without a daemon restart" reasoning.
fn resolved_thrash_thresholds() -> (u32, u32) {
    let cfg = crate::config::load_thrash_config();
    (cfg.max_compactions(), cfg.min_turn_gap())
}

impl RunnerSpec {
    /// Build a spec from a stored cell row.
    ///
    /// When the session declares `subprojects`, a system-prompt addendum is
    /// injected to tell the agent to confine its edits to those subdirectories
    /// (RAL-23). The addendum is appended after any user-supplied system prompt.
    #[must_use]
    pub fn from_row(squad_id: &str, row: &CellRow) -> Self {
        let subproject_addendum = subproject_system_prompt_addendum(&row.subprojects);
        let system_prompt = match (row.system_prompt.clone(), subproject_addendum) {
            (Some(existing), Some(addendum)) => {
                let combined = format!("{existing}\n\n{addendum}");
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                crate::rlog!(
                    DEBUG,
                    "ralphus [spec] cell {} system-prompt: user-supplied ({} chars) + subproject addendum → combined ({} chars)",
                    row.cell_id,
                    existing.len(),
                    combined.len()
                );
                Some(combined)
            }
            (Some(existing), None) => {
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                crate::rlog!(
                    DEBUG,
                    "ralphus [spec] cell {} system-prompt: borrowed from cell config ({} chars)",
                    row.cell_id,
                    existing.len()
                );
                Some(existing)
            }
            (None, Some(addendum)) => {
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                crate::rlog!(
                    DEBUG,
                    "ralphus [spec] cell {} system-prompt: synthesised from subprojects ({} chars, {:?})",
                    row.cell_id,
                    addendum.len(),
                    row.subprojects
                );
                Some(addendum)
            }
            (None, None) => None,
        };
        // Carry the stored position through; when the subproject injection
        // creates a system prompt where none existed before, record "append" so
        // the wire contract is self-consistent.
        let system_prompt_position = row
            .system_prompt_position
            .clone()
            .or_else(|| system_prompt.as_ref().map(|_| "append".to_string()));
        let (thrash_max_compactions, thrash_min_turn_gap) = resolved_thrash_thresholds();
        let agent_isolation = crate::config::resolve_agent_isolation(std::path::Path::new(
            row.cwd.as_deref().unwrap_or(""),
        ));
        Self {
            squad_id: squad_id.to_string(),
            task: row.task_name.clone(),
            cell_id: row.cell_id.clone(),
            cwd: row.cwd.clone().unwrap_or_default(),
            prompt: row.prompt.clone(),
            command: row.command.clone(),
            agent: row.agent.clone(),
            executable: None,
            model: row.model.clone(),
            system_prompt,
            system_prompt_position,
            timeout_sec: row.timeout_sec.and_then(|s| u64::try_from(s).ok()),
            budget_tokens: row.budget_tokens.and_then(|b| u64::try_from(b).ok()),
            maximum_budget_usd: row.maximum_budget_usd,
            maximum_context: row.maximum_context.and_then(|v| u64::try_from(v).ok()),
            auto_compact_threshold: row
                .auto_compact_threshold
                .and_then(|v| u64::try_from(v).ok()),
            maximum_tool_output_tokens: row
                .maximum_tool_output_tokens
                .and_then(|v| u64::try_from(v).ok()),
            proof: false,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            env_overrides: BTreeMap::new(),
            // RAL-185: carried from the row so the router can dispatch this
            // cell to its machine. `None` for every pre-RAL-185 row.
            machine: row.machine.clone(),
            tool_arg_truncate_chars: Some(resolved_tool_arg_truncate_chars()),
            thrash_max_compactions: Some(thrash_max_compactions),
            thrash_min_turn_gap: Some(thrash_min_turn_gap),
            allow_personal_settings: agent_isolation.allow_personal_settings(),
            allow_personal_memory: agent_isolation.allow_personal_memory(),
        }
    }

    /// Set the machine this spec runs on (RAL-185).
    ///
    /// A builder rather than a constructor parameter because the proof
    /// constructors below already take enough arguments, and a proof step's
    /// machine is always inherited from its owning cell/task rather than
    /// being independently derived here.
    #[must_use]
    pub fn with_machine(mut self, machine: Option<String>) -> Self {
        self.machine = machine;
        self
    }

    /// Build a spec for an `agent`-kind proof step. `agent`/`model` are the
    /// owning cell's resolved backend, with `model` overridden by the
    /// proof step's own `model` field when set.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn for_proof(
        squad_id: &str,
        task: &str,
        cell_id: &str,
        cwd: &str,
        prompt: &str,
        agent: &str,
        model: Option<&str>,
        timeout_sec: Option<u64>,
        budget_tokens: Option<u64>,
        // The proof step's resolved tool-output token cap (RAL-333), already
        // resolved against its real parent -- a task-scope proof against the
        // task, a cell-scope proof against its owning cell. Unlike
        // `maximum_context`/`auto_compact_threshold` below, proof steps do
        // carry this field (see `ralphus_core::schema::
        // resolve_task_proof_maximum_tool_output_tokens`/
        // `resolve_cell_proof_maximum_tool_output_tokens`).
        maximum_tool_output_tokens: Option<u64>,
    ) -> Self {
        let (thrash_max_compactions, thrash_min_turn_gap) = resolved_thrash_thresholds();
        let agent_isolation = crate::config::resolve_agent_isolation(std::path::Path::new(cwd));
        Self {
            squad_id: squad_id.to_string(),
            task: task.to_string(),
            cell_id: cell_id.to_string(),
            cwd: cwd.to_string(),
            prompt: Some(prompt.to_string()),
            command: None,
            agent: agent.to_string(),
            executable: None,
            model: model.map(str::to_string),
            // Proof steps do not carry a cell's appended system prompt.
            system_prompt: None,
            system_prompt_position: None,
            timeout_sec,
            budget_tokens,
            // Proof steps have no `maximum_budget_usd`/`maximum_context`/
            // `auto_compact_threshold` field of their own today (RAL-161
            // scoped the USD cap to cells/tasks only; RAL-304 follows the
            // same scope for the context-window/auto-compact caps).
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens,
            proof: true,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            env_overrides: BTreeMap::new(),
            machine: None,
            tool_arg_truncate_chars: Some(resolved_tool_arg_truncate_chars()),
            thrash_max_compactions: Some(thrash_max_compactions),
            thrash_min_turn_gap: Some(thrash_min_turn_gap),
            allow_personal_settings: agent_isolation.allow_personal_settings(),
            allow_personal_memory: agent_isolation.allow_personal_memory(),
        }
    }

    /// The full appended system prompt this runner spec will deliver to the
    /// backend, after ralphus adds its own non-interactive/async/proof/ghost
    /// instructions.
    #[must_use]
    pub fn effective_system_prompt(&self) -> Option<String> {
        self.prompt.as_ref()?;
        Some(if self.proof {
            effective_proof_system_prompt(self.system_prompt.as_deref())
        } else {
            combine_system_prompts([
                self.system_prompt.as_deref(),
                Some(NON_INTERACTIVE_SYSTEM_PROMPT),
                Some(TOOLS_SYSTEM_PROMPT),
                Some(ASYNC_SYSTEM_PROMPT),
                Some(GHOST_SYSTEM_PROMPT),
            ])
            .expect("prompt cells always include ralphus system instructions")
        })
    }

    /// Build a spec for a `command`-kind proof step (RAL-151). Wrapped in
    /// tmux exactly like every other cell/proof invocation, so it can be
    /// watched live and reuses the same peek/pane-capture endpoints a
    /// `prompt`-kind proof step already does — see
    /// `daemon/src/scheduler.rs::run_verifies`'s `"command"` branch.
    #[must_use]
    pub fn for_command_proof(
        squad_id: &str,
        task: &str,
        cell_id: &str,
        cwd: &str,
        command: &str,
        agent: &str,
        timeout_sec: Option<u64>,
    ) -> Self {
        Self {
            squad_id: squad_id.to_string(),
            task: task.to_string(),
            cell_id: cell_id.to_string(),
            cwd: cwd.to_string(),
            prompt: None,
            command: Some(command.to_string()),
            agent: agent.to_string(),
            executable: None,
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            timeout_sec,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            // Command-kind proof steps never reach a `ModelBackend`, so there
            // is no tool-output cap to configure here either.
            maximum_tool_output_tokens: None,
            proof: true,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            env_overrides: BTreeMap::new(),
            machine: None,
            // Command-kind proof steps never reach a `ModelBackend` (see
            // `runner::execute::run_cell`'s `command` branch), so there is no
            // tool-call rendering here to configure.
            tool_arg_truncate_chars: None,
            // Nor is there anything to compact -- no `ModelBackend` means no
            // compaction events at all.
            thrash_max_compactions: None,
            thrash_min_turn_gap: None,
            // Same rationale: no `ModelBackend` reached, so isolation is
            // inert here.
            allow_personal_settings: false,
            allow_personal_memory: false,
        }
    }
}

/// The JSON result read from the runner's stdout (mirrors the runner's `CellResult`).
#[derive(Debug, Clone, Deserialize)]
pub struct RunnerResult {
    /// `"done"` or `"failed"`.
    pub status: String,
    /// Input tokens used.
    #[serde(default)]
    pub tokens_in: i64,
    /// Output tokens used.
    #[serde(default)]
    pub tokens_out: i64,
    /// RAL-326: prompt-cache *write* tokens -- input billed at the
    /// cache-creation rate. Deliberately not folded into `tokens_in`, which
    /// keeps meaning "uncached input". `0` for a backend whose harness
    /// reports no cache breakdown.
    #[serde(default)]
    pub cache_creation_tokens: i64,
    /// RAL-326: prompt-cache *read* tokens -- input served from an existing
    /// cache entry. See `cache_creation_tokens`.
    #[serde(default)]
    pub cache_read_tokens: i64,
    /// RAL-373: total input tokens spent on Claude Code's own
    /// auto-compaction summarization requests, billed at the *uncached*
    /// input rate -- the reason `cost_usd` and
    /// `tokens_in + cache_creation_tokens + cache_read_tokens` diverge on
    /// any cell that compacts. Compaction *output* (the summary itself) is
    /// not currently reported by Claude Code and so is not captured here.
    /// `0` for a backend that reports no compaction data (`pi`, `codex`) --
    /// see `compaction_count` before reading that as "never compacted".
    #[serde(default)]
    pub compaction_input_tokens: i64,
    /// RAL-373: count of compactions observed, incremented independently of
    /// whether each one's input size was reported. A nonzero count paired
    /// with `compaction_input_tokens == 0` means "compactions happened,
    /// sizes unreported by this backend/version", not "no compaction
    /// happened".
    #[serde(default)]
    pub compaction_count: i64,
    /// Cost in USD.
    #[serde(default)]
    pub cost_usd: f64,
    /// RAL-326: `true` when the usage figures above are the last live
    /// mid-run snapshot rather than the backend's own terminal accounting --
    /// the cell's process was lost, cancelled, timed out, or killed before a
    /// final usage event arrived. The board badges such a row as estimated so
    /// a snapshot is never read as a settled bill.
    #[serde(default)]
    pub cost_is_estimated: bool,
    /// Short summary of what happened.
    #[serde(default)]
    pub summary: String,
    /// Error detail when failed.
    #[serde(default)]
    pub error: Option<String>,
    /// For an `agent`-kind proof run: the parsed PASS/FAIL verdict. `None`
    /// for a normal cell, or when the proof ran but produced no
    /// parseable verdict (the runner treats that case as a fail already, so
    /// this is `Some(false)` far more often than `None` in practice).
    #[serde(default)]
    pub proofed: Option<bool>,
    /// The Claude Code session UUID emitted by the claude-code backend, for
    /// `claude --resume`. `None` for non-claude-code cells or when the
    /// runner could not extract it.
    #[serde(default)]
    pub agent_session_id: Option<String>,
    /// RAL-136: the agent's self-summarized handoff note ("ghost"), extracted
    /// from a `RALPHUS_GHOST:` marker in a normal (non-proof) prompt
    /// cell's final response. `None` for command cells, proof steps,
    /// or when the agent had nothing to hand off.
    #[serde(default)]
    pub ghost: Option<String>,
}

impl RunnerResult {
    /// A synthetic failure (e.g. the runner process could not be spawned).
    #[must_use]
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            status: "failed".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            compaction_input_tokens: 0,
            compaction_count: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: String::new(),
            error: Some(error.into()),
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }

    /// A cell killed mid-run for exceeding its configured
    /// `maximum_budget_usd` cap (RAL-161). Carries the last-known live
    /// tokens/cost rather than zeroing them out like [`Self::failure`] does --
    /// `record_cell_result`'s write of `tokens_in`/`tokens_out`/`cost_usd`
    /// is a plain overwrite (not a `COALESCE`), so a zeroed failure result
    /// would regress the board's already-live numbers back to `$0.0000` on
    /// the final write.
    #[must_use]
    pub fn cost_exceeded(usage: LiveUsage, cap: f64) -> Self {
        let cost_usd = usage.cost_usd;
        Self {
            status: "failed".to_string(),
            tokens_in: usage.tokens_in,
            tokens_out: usage.tokens_out,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            // RAL-373: `LiveUsage` carries no compaction breakdown (a live
            // mid-run snapshot, not the backend's terminal accounting) --
            // recorded as zero rather than guessed, same as every other
            // snapshot-derived constructor here.
            compaction_input_tokens: 0,
            compaction_count: 0,
            cost_usd,
            // The cap fires off a live snapshot, never a terminal usage
            // event, so what it records is an estimate by construction
            // (RAL-326).
            cost_is_estimated: true,
            summary: String::new(),
            error: Some(format!(
                "terminated: cost ${cost_usd:.4} exceeded maximum_budget_usd cap ${cap:.4}"
            )),
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }

    /// RAL-288 Stage 6: a deliberate human-triggered detach mid-task, so a
    /// real interactive session can safely take over. Carries whatever
    /// usage/session-id was captured live up to the detach point, mirroring
    /// [`Self::cost_exceeded`]'s reasoning -- the board's numbers must not
    /// regress to zero just because the cell paused rather than finished.
    #[must_use]
    pub fn detached(usage: LiveUsage, agent_session_id: Option<String>) -> Self {
        Self {
            status: "detached".to_string(),
            tokens_in: usage.tokens_in,
            tokens_out: usage.tokens_out,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            // RAL-373: same reasoning as `cost_exceeded` -- no compaction
            // breakdown in a live snapshot.
            compaction_input_tokens: 0,
            compaction_count: 0,
            cost_usd: usage.cost_usd,
            // Same reasoning as `cost_exceeded`: a detach point is a live
            // snapshot, not a final accounting (RAL-326).
            cost_is_estimated: true,
            summary: String::new(),
            error: None,
            proofed: None,
            agent_session_id,
            ghost: None,
        }
    }

    /// Whether the cell succeeded.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.status == "done"
    }

    /// RAL-288 Stage 6: a deliberate human-triggered detach mid-task --
    /// neither success nor failure. The scheduler special-cases this before
    /// it would otherwise reach [`Self::node_state`] (mirroring how a
    /// cancellation landing mid-run is handled), so this mapping only
    /// matters as a safety net against a future caller that doesn't.
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.status == "detached"
    }

    /// Map to a node state.
    #[must_use]
    pub fn node_state(&self) -> NodeState {
        if self.is_done() {
            NodeState::Done
        } else if self.is_detached() {
            NodeState::Running
        } else {
            NodeState::Failed
        }
    }

    /// Whether an `agent`-kind proof step passed: the runner itself must
    /// have completed (not crashed) *and* reported a true verdict. A proof
    /// that ran but could not be parsed for a verdict, or that crashed
    /// outright, counts as not-passed — fail closed.
    #[must_use]
    pub fn proof_passed(&self) -> bool {
        self.is_done() && self.proofed.unwrap_or(false)
    }
}

/// Something that can execute a cell. Send + Sync so worker threads can share it.
pub trait Runner: Send + Sync {
    /// Execute a cell and report its result.
    fn run(&self, spec: &RunnerSpec) -> RunnerResult;

    /// Check whether an agent can be dispatched before mutating a worktree.
    ///
    /// Test runners deliberately accept every agent: they do not spawn an
    /// external process, so requiring their host to have a CLI installed would
    /// make in-process merge tests depend on local developer tooling.
    fn preflight_agent(
        &self,
        _agent: &str,
        _executable: Option<&str>,
        _machine: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Check that the runner executable itself (`ralphus-runner`, resolved
    /// via `RALPHUS_RUNNER_CMD`) exists and is runnable, before the scheduler
    /// marks a cell in-progress (RAL-377). `machine` mirrors
    /// [`Runner::preflight_agent`]'s parameter of the same name: `None` (or
    /// a machine that resolves local) checks this daemon's own
    /// `RALPHUS_RUNNER_CMD`; a machine that routes to a remote provider is
    /// not checked at all, since this daemon has no visibility into a
    /// remote host's `PATH`.
    ///
    /// Deliberately distinct from [`Runner::preflight_agent`], which checks
    /// the *backend* CLI (`claude`, `codex`, ...) a spawned `ralphus-runner`
    /// process would in turn invoke -- and does so by actually spawning
    /// `ralphus-runner` itself, so it can never distinguish "the backend CLI
    /// is missing" from "`ralphus-runner` itself is missing" (the latter
    /// currently isn't even called for a normal cell, only for a guardian
    /// resolver dispatch). This check exists to catch exactly that second,
    /// narrower case up front, without expanding scope to every executable
    /// the daemon might shell out to.
    ///
    /// Test runners deliberately accept unconditionally, mirroring
    /// `preflight_agent`'s rationale: they never spawn `ralphus-runner` at
    /// all, so requiring a real one on `PATH` would make in-process
    /// scheduler tests depend on local build artifacts.
    fn preflight_runner_executable(&self, _machine: Option<&str>) -> Result<(), String> {
        Ok(())
    }

    /// Like [`run`](Runner::run), but aborts — killing any spawned
    /// subprocess — as soon as `cancel` trips. The default ignores
    /// cancellation and just calls [`run`](Runner::run); the real
    /// [`SubprocessRunner`] overrides it to poll the token and kill its child.
    fn run_cancellable(&self, spec: &RunnerSpec, _cancel: &CancelToken) -> RunnerResult {
        self.run(spec)
    }
}

/// Runs a cell by spawning the configured runner program and speaking JSON
/// over stdin/stdout.
pub struct SubprocessRunner {
    program: String,
    args: Vec<String>,
    /// When set, each spawned child's PID is registered here for the lifetime of
    /// the cell so the resource view can attribute OS metrics to it (RAL-11).
    registry: Option<ProcRegistry>,
    /// When set, `RALPHUS_EVENT:` marker lines on the child's stderr are
    /// parsed and forwarded into Cartographer (RAL-98).
    cartographer: Option<Arc<Mutex<Store>>>,
    /// When set, a per-cell detach token is registered for the lifetime of
    /// each tmux-wrapped attempt so an HTTP handler can request a clean
    /// mid-task detach (RAL-288 Stage 6) via the same registry, without
    /// touching the rest of that cell's squad.
    detachments: Option<crate::cancel::Detachments>,
}

impl SubprocessRunner {
    /// Build from a command line like `"ralphus-runner"` or `"python -m ralphus.runner"`.
    #[must_use]
    pub fn new(command_line: &str) -> Self {
        let mut parts = command_line.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "ralphus-runner".to_string());
        Self {
            program,
            args: parts.collect(),
            registry: None,
            cartographer: None,
            detachments: None,
        }
    }

    /// Resolve the runner command from `RALPHUS_RUNNER_CMD`, defaulting to
    /// `ralphus-runner`.
    #[must_use]
    pub fn from_env() -> Self {
        let cmd =
            std::env::var("RALPHUS_RUNNER_CMD").unwrap_or_else(|_| "ralphus-runner".to_string());
        Self::new(&cmd)
    }

    /// Attach a process registry so spawned children are tracked for the
    /// resource-usage endpoint (RAL-11).
    #[must_use]
    pub fn with_registry(mut self, registry: ProcRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Attach a store handle so `RALPHUS_EVENT:` marker lines on the child's
    /// stderr are forwarded into Cartographer (RAL-98).
    #[must_use]
    pub fn with_cartographer(mut self, store: Arc<Mutex<Store>>) -> Self {
        self.cartographer = Some(store);
        self
    }

    /// Attach the shared per-cell detach registry (RAL-288 Stage 6) so a
    /// tmux-wrapped cell can be cleanly detached mid-task via an HTTP
    /// handler holding the same registry.
    #[must_use]
    pub fn with_detachments(mut self, detachments: crate::cancel::Detachments) -> Self {
        self.detachments = Some(detachments);
        self
    }
}

/// Registers a cell's subprocess PID on construction and unregisters it on
/// drop, so the resource view never sees a PID after the process has exited —
/// whatever exit path the runner takes (success, timeout, cancel, error).
struct PidGuard<'a> {
    registry: &'a ProcRegistry,
    squad_id: &'a str,
    cell_id: &'a str,
}

impl Drop for PidGuard<'_> {
    fn drop(&mut self) {
        self.registry.unregister(self.squad_id, self.cell_id);
    }
}

/// Removes a cell's detach-token registration on every return path out of
/// [`SubprocessRunner::run_via_tmux`] (RAL-288 Stage 6), the same way
/// [`PidGuard`] removes a PID registration -- a token left registered after
/// its run finished would let a stale HTTP detach request silently no-op
/// against a run that already ended, never against something still live,
/// but there's no reason to let the registry grow unbounded either.
struct DetachGuard<'a> {
    detachments: &'a crate::cancel::Detachments,
    session_name: &'a str,
}

impl Drop for DetachGuard<'_> {
    fn drop(&mut self) {
        self.detachments.remove(self.session_name);
    }
}

/// How often a tmux-wrapped runner invocation's pane is polled for the
/// completion sentinel (RAL-102). Deliberately coarser than a plain child-
/// exit poll would be, since each tick costs a real `tmux capture-pane`
/// subprocess spawn, not just a syscall.
const TMUX_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// RAL-241: how long a tmux-wrapped session may show no pane growth before a
/// `high`-priority mailbox stall escalation fires (see
/// `SubprocessRunner::check_stall_escalation`). Overridable via
/// `RALPHUS_MAILBOX_STALL_SECS` — the ticket's own manual-verification repro
/// task needs a window far shorter than the 5-real-minute default to be
/// practical to sit and watch.
fn mailbox_stall_threshold() -> Duration {
    std::env::var("RALPHUS_MAILBOX_STALL_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5 * 60))
}

/// Sentinel prefix the runner prints to its own stdout (which lands in the
/// tmux pane, not a pipe the daemon reads) once it has written its
/// `CellResult` to the `--result-file` it was given. Mirrors the
/// `RALPHUS_PROOF:`/`RALPHUS_EVENT:` marker idiom, just polled from pane
/// content instead of a stderr pipe (RAL-102 Q2/Q3/Q5).
const TMUX_DONE_MARKER: &str = "RALPHUS_TMUX_DONE";

/// Whether `pane`'s captured content shows the *real* completion sentinel —
/// a standalone line of the exact form `RALPHUS_TMUX_DONE: <status>`
/// (`cli/src/ralphus/runner/__main__.py`'s `_finish`), not merely the
/// marker text appearing anywhere in the buffer.
///
/// A plain `pane.contains(TMUX_DONE_MARKER)` substring scan false-positives
/// whenever the cell's own work happens to echo the literal marker —
/// e.g. an agent whose task is about this exact tmux-completion mechanism
/// `Read`ing or `Grep`ing `runner.rs`/`__main__.py`, both of which contain
/// the string `RALPHUS_TMUX_DONE` in their own source. That premature
/// "done" makes the daemon try to read a result file the real subprocess
/// hasn't written yet, fail with "no result file", and then kill the
/// cell's tmux pane out from under work that was still genuinely in
/// progress — observed in production repeatedly and *only* for a
/// live-view/tmux-themed ticket, never for unrelated ones in the same
/// batch. Requiring the marker to be the start of its own line (as
/// `print(f"{TMUX_DONE_MARKER}: {status}")` always produces, flush left,
/// nothing else on the line) is not proof against a maximally adversarial
/// pane transcript, but it is proof against every real false-positive
/// observed so far: a grep match is prefixed with `file:line:`, a `Read`
/// tool's numbered listing is prefixed with a line number, and quoting the
/// marker in prose reads as ordinary text, not a line starting with it.
fn pane_shows_done_sentinel(pane: &str) -> bool {
    let prefix = format!("{TMUX_DONE_MARKER}: ");
    pane.lines()
        .any(|line| line.trim_start().starts_with(&prefix))
}

impl Runner for SubprocessRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        self.run_cancellable(spec, &CancelToken::never())
    }

    /// Every spec — `prompt`-kind (agent invocations: normal task cells,
    /// `agent`-kind proof steps, and Guardian merge/resolver/synthesizer
    /// cells) and `command`-kind (deterministic shell cells/proof
    /// steps) alike — runs tmux-wrapped so the board can show a live view.
    /// `command`-kind specs joined this blanket, no-opt-in path in RAL-151;
    /// before that (RAL-102) only `prompt`-kind specs ran under tmux and a
    /// `command`-kind spec ran as a raw child process instead, with no live
    /// view available for it.
    fn run_cancellable(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        self.run_via_tmux(spec, cancel)
    }

    fn preflight_agent(
        &self,
        agent: &str,
        executable: Option<&str>,
        _machine: Option<&str>,
    ) -> Result<(), String> {
        let mut command = std::process::Command::new(&self.program);
        command
            .args(&self.args)
            .arg("preflight")
            .arg("--agent")
            .arg(agent);
        if let Some(executable) = executable {
            command.arg("--executable").arg(executable);
        }
        let output = command
            .output()
            .map_err(|error| format!("could not start ralphus-runner preflight: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if detail.is_empty() {
            format!("ralphus-runner preflight exited with {}", output.status)
        } else {
            detail
        })
    }

    fn preflight_runner_executable(&self, _machine: Option<&str>) -> Result<(), String> {
        crate::agent_profiles::resolve_executable(&self.program)
            .map(|_resolved| ())
            .map_err(|reason| {
                format!(
                    "runner executable \"{}\" (resolved from RALPHUS_RUNNER_CMD, default \
                     \"ralphus-runner\") is missing or not runnable: {reason}",
                    self.program
                )
            })
    }
}

impl SubprocessRunner {
    /// Bounded retries for a cell whose tmux pane vanishes unexpectedly
    /// mid-run — the still-unresolved "mystery session death" investigated at
    /// length in `PSMUX_CRASH_NOTES.local.md`. One confirmed finding there is
    /// that the underlying backing process can survive even after the daemon
    /// loses tmux-level visibility into it, so a hard failure on the first
    /// missed poll can discard genuinely-still-running work. Only exercised
    /// for agents with a real resume mechanism (`claude-code`/`claude-cli`,
    /// `codex`/`codex-cli`), and only once a `agent_session_id` has actually
    /// been captured live from the pane — both required for the backend's
    /// resume command to have anything to resume. Kept small: this
    /// mitigates an unreliable pane-tracking layer, it is not a substitute for
    /// genuine failure — a cell that keeps dying even after being resumed
    /// still fails, it does not retry forever.
    const MAX_REATTACH_ATTEMPTS: u32 = 2;

    /// Run `spec` inside a tmux session, transparently retrying via the
    /// backend's own resume mechanism (`claude -p --resume <id>` for
    /// `claude-code`/`claude-cli`, `codex exec resume <id>` for
    /// `codex`/`codex-cli`) in a fresh tmux session (same deterministic
    /// name, so the board's existing "Show Live View"/"Open Terminal Log"
    /// buttons — keyed by `(squad_id, task, cell_id)` — transparently start
    /// working again once a reattach succeeds, no UI changes needed) if the
    /// pane vanishes unexpectedly mid-run. See [`Self::MAX_REATTACH_ATTEMPTS`]
    /// for why this is bounded, and `PSMUX_CRASH_NOTES.local.md` for the
    /// underlying investigation this mitigates rather than fixes.
    ///
    /// The wall-clock timeout budget (`spec.timeout_sec`) is shared across
    /// every attempt, not reset per attempt — a reattach must never let a
    /// cell run longer in total than it was configured to. Cancellation
    /// and a genuine timeout are never reattach-eligible; only a pane that
    /// vanished on its own (confirmed via `MISSING_SESSION_STRIKE_LIMIT`
    /// consecutive misses) is.
    ///
    /// Every stage is logged (both `rlog!` and a Cartographer breadcrumb via
    /// [`Self::emit_reattach_note`]) with as much state as is available —
    /// attempt number, elapsed time, the captured `agent_session_id`, why a
    /// reattach was or wasn't attempted, and the eventual outcome — so a real
    /// occurrence is fully diagnosable after the fact from Cartographer alone.
    fn run_via_tmux(&self, spec: &RunnerSpec, cancel: &CancelToken) -> RunnerResult {
        let tmux = match Tmux::resolve() {
            Ok(t) => t,
            Err(e) => return RunnerResult::failure(format!("could not resolve tmux: {e}")),
        };

        let io_dir = std::env::temp_dir().join("ralphus-runner-io");
        if let Err(e) = std::fs::create_dir_all(&io_dir) {
            return RunnerResult::failure(format!("could not create runner IO dir: {e}"));
        }
        // `key` (from `tmux::session_name`) is already the fully-formed,
        // "ralphus_"-prefixed session name — do not re-prefix it. A prior
        // double-prefix bug meant the tmux session actually created never
        // matched the name `capture_pane_reply`/`attach_tmux_terminal`
        // recomputed via a bare `tmux::session_name` call, so the live
        // "peek"/"open terminal" endpoints always reported the session as
        // inactive even while it was running. Reused unchanged across every
        // reattach attempt below — see this fn's doc comment.
        let session_name = crate::tmux::session_name(&spec.squad_id, &spec.task, &spec.cell_id);
        let spec_path = io_dir.join(format!("{session_name}.spec.json"));
        let result_path = io_dir.join(format!("{session_name}.result.json"));
        // Loaded once per cell run (not per attempt/poll) — RAL-154.
        let terminal_log_max_lines =
            crate::config::load_terminal_log_config().max_lines_per_attempt();

        // Shared across every attempt: the overall wall-clock budget must not
        // reset on a reattach, and the agent_session_id captured live on one
        // attempt is exactly what the next attempt resumes from.
        let started = Instant::now();
        let deadline = spec.timeout_sec.map(Duration::from_secs);
        let mut resumable_agent_session_id: Option<String> = None;
        let mut attempt: u32 = 0;

        // RAL-288 Stage 6: registered for the lifetime of this whole cell run
        // (every reattach attempt below shares it, not a fresh one per
        // attempt) so an HTTP handler can request a clean mid-task detach via
        // the same registry at any point, keyed by the same deterministic
        // `session_name` the board's live-view/attach endpoints already use.
        // `_detach_guard` removes it on every return path via `Drop`, the
        // same pattern `PidGuard` already uses below for the same reason.
        let detach_token = self.detachments.as_ref().map(|d| d.register(&session_name));
        let _detach_guard = self.detachments.as_ref().map(|d| DetachGuard {
            detachments: d,
            session_name: &session_name,
        });

        loop {
            let attempt_spec: std::borrow::Cow<'_, RunnerSpec> = if attempt == 0 {
                std::borrow::Cow::Borrowed(spec)
            } else {
                let mut s = spec.clone();
                s.resume_agent_session_id = resumable_agent_session_id.clone();
                std::borrow::Cow::Owned(s)
            };

            if attempt > 0 {
                crate::rlog!(
                    WARNING,
                    "ralphus [runner] reattach attempt {attempt}/{} squad={} cell={} task={} resuming agent_session_id={:?}",
                    Self::MAX_REATTACH_ATTEMPTS,
                    spec.squad_id,
                    spec.cell_id,
                    spec.task,
                    resumable_agent_session_id,
                );
                self.emit_reattach_note(
                    spec,
                    "reattach attempt starting",
                    crate::logging::LogLevel::WARNING,
                    attempt,
                    resumable_agent_session_id.as_deref(),
                    None,
                    started.elapsed().as_secs(),
                    None,
                );
            }

            let (result, session_died_unexpectedly) = self.run_via_tmux_attempt(
                &attempt_spec,
                cancel,
                detach_token.as_ref(),
                &tmux,
                &session_name,
                &spec_path,
                &result_path,
                &started,
                deadline,
                &mut resumable_agent_session_id,
                attempt,
                terminal_log_max_lines,
            );

            if !session_died_unexpectedly {
                crate::rlog!(
                    INFO,
                    "ralphus [runner] tmux done squad={} cell={} status={} tokens_in={} tokens_out={} cost_usd={:.4} attempts={}",
                    spec.squad_id,
                    spec.cell_id,
                    result.status,
                    result.tokens_in,
                    result.tokens_out,
                    result.cost_usd,
                    attempt + 1,
                );
                self.clear_live_activity(&session_name);
                return result;
            }

            // Session vanished on its own this attempt — decide whether a
            // reattach is possible, logging full state either way (this is
            // the "what was lost, what was retried, and why" breadcrumb
            // trail this mechanism exists to leave behind).
            let already_timed_out = timed_out(started.elapsed(), deadline);
            let reason = if !matches!(
                attempt_spec.agent.as_str(),
                "claude-code" | "claude-cli" | "codex" | "codex-cli" | "pi"
            ) {
                "agent does not support resume"
            } else if resumable_agent_session_id.is_none() {
                "no agent_session_id was ever captured for this cell"
            } else if already_timed_out {
                "timeout budget exhausted"
            } else if attempt >= Self::MAX_REATTACH_ATTEMPTS {
                "reattach attempts exhausted"
            } else {
                "reattach possible"
            };
            let can_reattach = reason == "reattach possible";
            crate::rlog!(
                WARNING,
                "ralphus [runner] session lost squad={} cell={} task={} attempt={}/{} agent={} elapsed_secs={} resumable_agent_session_id={:?} will_reattach={can_reattach} reason={reason:?} error={:?}",
                spec.squad_id,
                spec.cell_id,
                spec.task,
                attempt,
                Self::MAX_REATTACH_ATTEMPTS,
                attempt_spec.agent,
                started.elapsed().as_secs(),
                resumable_agent_session_id,
                result.error,
            );
            self.emit_reattach_note(
                spec,
                "session lost",
                crate::logging::LogLevel::WARNING,
                attempt,
                resumable_agent_session_id.as_deref(),
                Some(reason),
                started.elapsed().as_secs(),
                result.error.as_deref(),
            );

            if !can_reattach {
                crate::rlog!(
                    ERROR,
                    "ralphus [runner] giving up on session squad={} cell={} after {} attempt(s): {reason}",
                    spec.squad_id,
                    spec.cell_id,
                    attempt + 1,
                );
                self.emit_reattach_note(
                    spec,
                    "giving up, no further reattach",
                    crate::logging::LogLevel::ERROR,
                    attempt,
                    resumable_agent_session_id.as_deref(),
                    Some(reason),
                    started.elapsed().as_secs(),
                    result.error.as_deref(),
                );
                let mut result = result;
                if attempt > 0 {
                    let note =
                        format!("session lost after {attempt} reattach attempt(s) ({reason})");
                    result.error = Some(match result.error.take() {
                        Some(existing) => format!("{existing}\n{note}"),
                        None => note,
                    });
                }
                self.clear_live_activity(&session_name);
                return result;
            }

            attempt += 1;
        }
    }

    /// Run one attempt of a tmux-wrapped invocation of `attempt_spec`: write
    /// it to `spec_path`, kill any stale session named `session_name`, spawn
    /// a fresh one, and poll until it completes, is cancelled, times out
    /// (against the *shared* `started`/`deadline` from [`Self::run_via_tmux`]
    /// — never reset per attempt, so a reattach cannot extend a cell's
    /// configured timeout budget), or is confirmed dead
    /// (`MISSING_SESSION_STRIKE_LIMIT` consecutive `has-session` misses).
    ///
    /// Every "session-id known" event seen in the pane updates
    /// `resumable_agent_session_id` in place, so a subsequent attempt can
    /// resume the latest known conversation even if this one never finished.
    ///
    /// Returns `(result, session_died_unexpectedly)` — the latter is `true`
    /// only on the "vanished on its own" path, never for an intentional
    /// cancel/timeout, so [`Self::run_via_tmux`] can tell a genuine
    /// mystery-death apart from a deliberate stop. A test caught this:
    /// `live_tmux_run_via_tmux_is_cancellable` asserts the error is exactly
    /// `"cancelled"` with no exit-code diagnostic appended.
    #[allow(clippy::too_many_arguments)]
    fn run_via_tmux_attempt(
        &self,
        attempt_spec: &RunnerSpec,
        cancel: &CancelToken,
        detach: Option<&crate::cancel::DetachToken>,
        tmux: &Tmux,
        session_name: &str,
        spec_path: &std::path::Path,
        result_path: &std::path::Path,
        started: &Instant,
        deadline: Option<Duration>,
        resumable_agent_session_id: &mut Option<String>,
        attempt: u32,
        terminal_log_max_lines: usize,
    ) -> (RunnerResult, bool) {
        // A stale file from a prior crashed/killed attempt of the same
        // (squad_id, task, cell_id) must never be mistaken for this
        // attempt's result.
        let _ = std::fs::remove_file(result_path);

        let payload = match serde_json::to_string(attempt_spec) {
            Ok(p) => p,
            Err(e) => {
                return (
                    RunnerResult::failure(format!("could not serialize spec: {e}")),
                    false,
                );
            }
        };
        if let Err(e) = std::fs::write(spec_path, payload) {
            return (
                RunnerResult::failure(format!("could not write spec file: {e}")),
                false,
            );
        }

        let mut all_args = self.args.clone();
        all_args.push("send".to_string());
        all_args.push(spec_path.to_string_lossy().into_owned());
        all_args.push("--result-file".to_string());
        all_args.push(result_path.to_string_lossy().into_owned());

        // A tmux session with this exact deterministic name can already exist
        // and still be alive: tmux sessions are owned by the tmux server, not
        // the daemon, so they survive a daemon crash/restart — or, on a
        // reattach, may be the very session this attempt is replacing (killed
        // just below, before this loop reaches this point again). Resuming
        // this (squad_id, task, cell_id) slot without killing it first would
        // otherwise collide with that existing session (`new-session` errors
        // on a duplicate name), failing this attempt immediately.
        if tmux.has_session(session_name) {
            crate::rlog!(
                WARNING,
                "ralphus [runner] killing stale tmux session {session_name} before starting a fresh one squad={} cell={}",
                attempt_spec.squad_id,
                attempt_spec.cell_id,
            );
            // RAL-154: on a reattach, this stale session is the prior
            // attempt's own pane, possibly still producing output after that
            // attempt's own final capture (e.g. the pane flickered
            // unreachable long enough to trip `MISSING_SESSION_STRIKE_LIMIT`
            // and get treated as dead, but never actually died — see
            // PSMUX_CRASH_NOTES.local.md). Grab one last capture and fold it
            // into that outgoing attempt's durable log *before* killing it,
            // so this content is never silently discarded — previously the
            // prior attempt's log only ever reflected whatever `last_pane`
            // held as of its own last successful poll. Not meaningful for the
            // very first attempt (`attempt == 0`): a stale session found
            // there belongs to no attempt this call has ever tracked (most
            // likely a leftover from a crashed daemon's earlier lifetime),
            // and the incoming fresh attempt 0 would immediately overwrite
            // any salvage written to that same slot anyway.
            if attempt > 0 {
                if let Ok(content) = tmux.capture_pane(session_name, 10_000) {
                    crate::terminal_log::write_attempt(
                        session_name,
                        attempt - 1,
                        &content,
                        terminal_log_max_lines,
                    );
                    self.emit_terminal_log_note(attempt_spec, session_name, attempt - 1);
                }
            }
            let _ = tmux.kill_session(session_name);
        }
        // RAL-397 Phase 2C: a durable, unbounded-depth transcript of this
        // attempt's raw pane output, written continuously via `pipe_pane` --
        // see `Tmux::new_detached_session_with_command`'s doc comment. Not
        // yet consumed by anything in this pass (Phase 2D, which moves
        // event/sentinel reading onto this file, is deliberately deferred);
        // for now this only feeds Phase 2E's terminal-log derivation.
        let transcript_path = crate::terminal_log::raw_transcript_path(session_name, attempt);
        if let Err(e) = tmux.new_detached_session_with_command(
            session_name,
            &attempt_spec.cwd,
            &attempt_spec.env_overrides,
            &self.program,
            &all_args,
            Some(&transcript_path),
        ) {
            let _ = std::fs::remove_file(spec_path);
            return (
                RunnerResult::failure(format!("could not start tmux session: {e}")),
                false,
            );
        }
        crate::rlog!(
            INFO,
            "ralphus [runner] tmux session started {session_name} squad={} cell={} agent={} model={} resume={:?}",
            attempt_spec.squad_id,
            attempt_spec.cell_id,
            attempt_spec.agent,
            attempt_spec.model.as_deref().unwrap_or("default"),
            attempt_spec.resume_agent_session_id,
        );
        self.emit_tmux_note(attempt_spec, "tmux session started", session_name);
        // Best-effort: if the session dies with no explanation (the ongoing
        // mystery -- see PSMUX_CRASH_NOTES.local.md), this is the one way to
        // learn whether its process actually crashed (a real Windows
        // exception code) or exited cleanly, without needing admin rights.
        // `None` (PID lookup failed, or non-Windows) just means no exit-code
        // detail is available later -- never fatal to the session itself.
        let server_pid = crate::tmux::find_server_pid(session_name);
        let exit_watch = server_pid.map(crate::tmux::watch_for_exit);
        // Register the same PID for the resource-usage view (RAL-11) so a
        // tmux-wrapped session (every cell/proof step, since RAL-151)
        // still shows up there — the raw-child-process path this used to
        // come from (`PidGuard` around a directly spawned `Command`) no
        // longer exists now that nothing runs outside tmux. Best-effort and
        // Windows-only, same caveats as `find_server_pid` itself; on any
        // other platform (or if the lookup races the just-spawned server)
        // this session simply doesn't appear in the resource view, exactly
        // as every `prompt`-kind cell already didn't between RAL-102 and
        // this fix.
        let _pid_guard = match (self.registry.as_ref(), server_pid) {
            (Some(registry), Some(pid)) => {
                registry.register(&attempt_spec.squad_id, &attempt_spec.cell_id, pid);
                Some(PidGuard {
                    registry,
                    squad_id: &attempt_spec.squad_id,
                    cell_id: &attempt_spec.cell_id,
                })
            }
            _ => None,
        };

        // RAL-241: baseline for the stall-escalation check below, used only
        // until the first real activity is observed (`Store::live_activity_ms`
        // is `None` until then).
        let attempt_started_ms = crate::store::now_ms();
        let stall_threshold = mailbox_stall_threshold();
        let mut lines_seen: usize = 0;
        let mut last_pane: Option<String> = None;
        let mut missing_session_strikes: u32 = 0;
        // A session that appears gone must be confirmed gone across a couple
        // of consecutive polls (not acted on the first miss) — root cause not
        // yet pinned down, but this build's tmux alternative (psmux) has
        // shown transient `capture-pane`/`has-session` failures under real
        // load that clear on the very next poll. Requiring 3 consecutive
        // misses (~1.5s at the current poll interval) filters those out
        // without meaningfully delaying a genuine session death.
        const MISSING_SESSION_STRIKE_LIMIT: u32 = 3;
        // Set only when the loop breaks because the session vanished on its
        // own (the mystery this file's exit-code watcher exists for) — never
        // for an intentional `cancel()`/timeout kill, which also produce a
        // non-done `RunnerResult` but have a perfectly well-known cause and
        // should never get "tmux server process exit code: ..." appended to
        // their message.
        let mut session_died_unexpectedly = false;
        // RAL-161: the last live usage snapshot seen from a runner event, so
        // an over-budget kill can carry the real tokens/cost through to
        // `record_cell_result` instead of zeroing them out.
        let mut current_usage = LiveUsage::default();
        // RAL-161: when a cost cap is configured, this loop's own cadence
        // (the check itself is a cheap in-memory float comparison) is driven
        // by the configurable, tighter `[budget].poll_interval_ms` instead of
        // the coarser `TMUX_POLL_INTERVAL` below -- which stays fixed since
        // it gates a real `tmux capture-pane` subprocess spawn, not just a
        // comparison. Without a cap there's nothing to check faster, so the
        // loop keeps its original cadence exactly.
        let budget_poll_interval = attempt_spec
            .maximum_budget_usd
            .map(|_| crate::config::load_budget_config().poll_interval());
        let mut last_tmux_poll = Instant::now();
        let mut first_tick = true;
        let result = loop {
            if cancel.is_cancelled() {
                let _ = tmux.kill_session(session_name);
                crate::rlog!(
                    INFO,
                    "ralphus [runner] cancelled squad={} cell={}",
                    attempt_spec.squad_id,
                    attempt_spec.cell_id
                );
                break RunnerResult::failure("cancelled");
            }
            if detach.is_some_and(crate::cancel::DetachToken::is_cancelled) {
                let _ = tmux.kill_session(session_name);
                crate::rlog!(
                    INFO,
                    "ralphus [runner] detached squad={} cell={}",
                    attempt_spec.squad_id,
                    attempt_spec.cell_id
                );
                break RunnerResult::detached(current_usage, resumable_agent_session_id.clone());
            }
            if timed_out(started.elapsed(), deadline) {
                let _ = tmux.kill_session(session_name);
                let secs = deadline.map(|d| d.as_secs()).unwrap_or(0);
                crate::rlog!(
                    WARNING,
                    "ralphus [runner] timed out after {secs}s squad={} cell={}",
                    attempt_spec.squad_id,
                    attempt_spec.cell_id
                );
                break RunnerResult::failure(format!("timed out after {secs}s"));
            }
            if let Some(cap) = attempt_spec.maximum_budget_usd {
                if current_usage.cost_usd > cap {
                    let _ = tmux.kill_session(session_name);
                    crate::rlog!(
                        WARNING,
                        "ralphus [runner] cost ${:.4} exceeded maximum_budget_usd cap ${cap:.4}, killing squad={} cell={}",
                        current_usage.cost_usd,
                        attempt_spec.squad_id,
                        attempt_spec.cell_id
                    );
                    self.emit_tmux_note(
                        attempt_spec,
                        "tmux session killed: cost limit exceeded",
                        session_name,
                    );
                    break RunnerResult::cost_exceeded(current_usage, cap);
                }
            }
            if first_tick || last_tmux_poll.elapsed() >= TMUX_POLL_INTERVAL {
                first_tick = false;
                last_tmux_poll = Instant::now();
                self.check_stall_escalation(
                    attempt_spec,
                    session_name,
                    attempt_started_ms,
                    stall_threshold,
                );
                match tmux.capture_pane(session_name, 10_000) {
                    Ok(pane) => {
                        missing_session_strikes = 0;
                        let all_lines: Vec<&str> = pane.lines().collect();
                        if all_lines.len() > lines_seen {
                            self.note_live_activity(session_name);
                            for line in &all_lines[lines_seen..] {
                                if let Some(json) = line.trim_end().strip_prefix(EVENT_MARKER) {
                                    let fwd = forward_runner_event(
                                        self.cartographer.as_ref(),
                                        &attempt_spec.squad_id,
                                        &attempt_spec.cell_id,
                                        &attempt_spec.task,
                                        json,
                                    );
                                    if let Some(sid) = fwd.agent_session_id {
                                        *resumable_agent_session_id = Some(sid);
                                    }
                                    if let Some(usage) = fwd.live_usage {
                                        current_usage = usage;
                                    }
                                }
                            }
                            lines_seen = all_lines.len();
                        }
                        let done = pane_shows_done_sentinel(&pane);
                        last_pane = Some(pane);
                        if done {
                            break Self::read_tmux_result(result_path, last_pane.as_deref());
                        }
                    }
                    Err(_) => {
                        // The session may have died before printing the sentinel
                        // (e.g. the runner process crashed hard enough to tear
                        // down the pane) — or `has-session` itself may just be
                        // having a transient hiccup, so this isn't trusted on the
                        // first miss (see `MISSING_SESSION_STRIKE_LIMIT`).
                        if !tmux.has_session(session_name) {
                            missing_session_strikes += 1;
                            if missing_session_strikes >= MISSING_SESSION_STRIKE_LIMIT {
                                session_died_unexpectedly = true;
                                break Self::read_tmux_result(result_path, last_pane.as_deref());
                            }
                        } else {
                            missing_session_strikes = 0;
                        }
                    }
                }
            }
            std::thread::sleep(budget_poll_interval.unwrap_or(TMUX_POLL_INTERVAL));
        };

        // RAL-397 Phase 2C: best-effort clean stop before the forceful
        // process-tree kill below -- not load-bearing (the job-object
        // confinement `kill_session` relies on, RAL-321, already tears down
        // any pipe-sink child spawned into this session's tree), but gives
        // the sink a chance to notice EOF and exit tidily rather than being
        // killed outright. A no-op when this attempt never started a pipe
        // (transcript_path was `None`).
        tmux.stop_pipe_pane(session_name);
        let _ = tmux.kill_session(session_name);
        let _ = std::fs::remove_file(spec_path);
        let _ = std::fs::remove_file(result_path);
        self.emit_tmux_note(attempt_spec, "tmux session ended", session_name);
        // Persist the last thing this attempt's pane actually showed, so
        // "Show Live View"/"Open Terminal Log" can still display it as a
        // read-only historical record once the live session is gone —
        // otherwise it's simply lost the moment `has_session` goes false.
        // Unconditional: written for every outcome (done, failed, cancelled,
        // timed out), overwriting whatever an earlier attempt left behind.
        crate::tmux::write_pane_snapshot(session_name, last_pane.as_deref().unwrap_or(""));
        // RAL-154: also persist it as *this attempt's own* durable, never-
        // overwritten record (unlike the single-slot snapshot above, which
        // the next attempt/reattach will replace) — so a restarted cell's
        // full multi-attempt history stays individually accessible.
        crate::terminal_log::write_attempt(
            session_name,
            attempt,
            last_pane.as_deref().unwrap_or(""),
            terminal_log_max_lines,
        );
        self.emit_terminal_log_note(attempt_spec, session_name, attempt);

        // Only worth the (bounded) wait on a failure -- a clean completion
        // doesn't need the "was this a crash?" diagnostic. If the tracked
        // process is really gone (which it should be, since the loop above
        // just concluded the session ended), the watcher's own blocking wait
        // should already be finishing, so this rarely actually waits long.
        let mut result = result;
        // RAL-187: rescue the tokens this attempt really spent before it
        // failed — see `backfill_live_usage`.
        backfill_live_usage(&mut result, current_usage);
        if session_died_unexpectedly && !result.is_done() {
            if let Some(observation) =
                exit_watch.and_then(|rx| rx.recv_timeout(Duration::from_secs(2)).ok())
            {
                let detail = match observation {
                    crate::tmux::ProcessExit::Exited(code) => {
                        format!("tmux server process exit code: {code}")
                    }
                    crate::tmux::ProcessExit::Unknown(reason) => {
                        format!("tmux server process exit code: unknown ({reason})")
                    }
                };
                crate::rlog!(
                    WARNING,
                    "ralphus [runner] {detail} squad={} cell={}",
                    attempt_spec.squad_id,
                    attempt_spec.cell_id
                );
                result.error = Some(match result.error.take() {
                    Some(existing) => format!("{existing}\n{detail}"),
                    None => detail,
                });
            }
        }

        (result, session_died_unexpectedly)
    }

    /// Emit a tmux session-lifecycle Cartographer note. Per RAL-102, only
    /// these start/end transitions are logged — the `capture-pane` polling
    /// itself never is, so a long-running session doesn't flood
    /// `cartographer_events`.
    fn emit_tmux_note(&self, spec: &RunnerSpec, message: &str, session_name: &str) {
        let Some(store) = &self.cartographer else {
            return;
        };
        let Ok(guard) = store.lock() else { return };
        crate::cartographer::Note::new("runner")
            .squad(&spec.squad_id)
            .cell(&spec.cell_id)
            .task(&spec.task)
            .scope("tmux")
            .emit(
                &guard,
                format!("{message} ({session_name})"),
                serde_json::json!({"session_name": session_name}),
            );
    }

    /// Record fresh pane output for `session_name` (RAL-170 liveness signal
    /// — see `Store::note_live_activity`'s doc comment for why this is an
    /// in-memory `Store` field rather than a Cartographer note or DB column).
    /// A no-op when no cartographer store is attached (e.g. in unit tests
    /// that construct a bare `SubprocessRunner`) or the mutex is poisoned —
    /// mirrors [`Self::emit_tmux_note`]'s best-effort shape.
    fn note_live_activity(&self, session_name: &str) {
        let Some(store) = &self.cartographer else {
            return;
        };
        let Ok(mut guard) = store.lock() else { return };
        guard.note_live_activity(session_name, crate::store::now_ms());
    }

    /// Drop the RAL-170 liveness entry for `session_name` once
    /// [`Self::run_via_tmux`] has a terminal result for good — never after
    /// just one attempt, since a reattach reuses the same deterministic
    /// `session_name` and the entry should keep reflecting real activity
    /// across that gap, not read as "ended" for the moments in between.
    /// Also drops the RAL-241 stall-escalation debounce entry at the same
    /// point, for the same reason.
    fn clear_live_activity(&self, session_name: &str) {
        let Some(store) = &self.cartographer else {
            return;
        };
        let Ok(mut guard) = store.lock() else { return };
        guard.clear_live_activity(session_name);
        guard.clear_stall_escalated(session_name);
    }

    /// RAL-241: check whether `session_name` has shown no activity (pane
    /// growth, per `Store::live_activity_ms`) for at least `threshold`, and
    /// if so, broadcast a `high`-priority mailbox message once for this
    /// stall onset. Called from the same tmux-poll cadence gate
    /// `Self::run_via_tmux_attempt`'s loop already uses for real
    /// `capture_pane` work, so this adds one cheap mutex lock + comparison
    /// per poll, not per loop iteration. A no-op when no cartographer store
    /// is attached, mirroring every other best-effort helper in this file.
    /// `threshold` is taken as a parameter (computed once by the caller from
    /// [`mailbox_stall_threshold`]) rather than read here on every call, both
    /// to avoid a repeated env lookup per poll and so tests can exercise both
    /// branches without mutating real process environment (this workspace
    /// forbids `unsafe_code` outright, and `std::env::set_var`/`remove_var`
    /// are `unsafe` — see `runner/src/providers.rs::resolve_with`'s doc
    /// comment for the same pattern).
    fn check_stall_escalation(
        &self,
        spec: &RunnerSpec,
        session_name: &str,
        attempt_started_ms: i64,
        threshold: Duration,
    ) {
        let Some(store) = &self.cartographer else {
            return;
        };
        let threshold_ms = threshold.as_millis() as i64;
        let Ok(mut guard) = store.lock() else { return };
        // `None` (no pane growth observed yet this attempt) falls back to
        // when this attempt started, not epoch 0 -- otherwise a cell that
        // simply hasn't produced its first line of output yet would appear
        // to have been stalled since 1970 and escalate immediately.
        let last_activity_ms = guard
            .live_activity_ms(session_name)
            .unwrap_or(attempt_started_ms);
        if crate::store::now_ms().saturating_sub(last_activity_ms) < threshold_ms {
            return;
        }
        if guard.is_stall_escalated(session_name, last_activity_ms) {
            return;
        }
        let text = format!(
            "cell '{}' in task '{}' (squad {}) has been stalled for over {}s with no activity",
            spec.cell_id,
            spec.task,
            spec.squad_id,
            threshold_ms / 1000,
        );
        let entity_uri = guard.cell_entity_uri(&spec.squad_id, &spec.task, &spec.cell_id);
        if let Ok(message_id) = guard.enqueue_mailbox_message(
            crate::mailbox::MailboxPriority::High,
            &text,
            Some(&spec.squad_id),
            Some(&spec.task),
            Some(&spec.cell_id),
            entity_uri.as_deref(),
        ) {
            crate::cartographer::Note::new("runner")
                .level(crate::logging::LogLevel::WARNING)
                .squad(&spec.squad_id)
                .cell(&spec.cell_id)
                .task(&spec.task)
                .scope("mailbox")
                .emit(
                    &guard,
                    format!("mailbox message enqueued for stalled session ({session_name})"),
                    serde_json::json!({
                        "message_id": message_id,
                        "priority": "high",
                        "session_name": session_name,
                        "stall_secs": threshold_ms / 1000,
                    }),
                );
        }
        guard.note_stall_escalated(session_name, last_activity_ms);
    }

    /// Emit a Cartographer note recording that a durable terminal-log attempt
    /// file was (re)written (RAL-154's `crate::terminal_log::write_attempt`),
    /// carrying the file's path so RAL-155's uber-log-viewer
    /// (`crate::timeline`) can find and inline it without a separate lookup
    /// mechanism — see [`crate::cartographer::CartographerRow::log_path`].
    fn emit_terminal_log_note(&self, spec: &RunnerSpec, session_name: &str, attempt: u32) {
        let Some(store) = &self.cartographer else {
            return;
        };
        let Ok(guard) = store.lock() else { return };
        let path = crate::terminal_log::attempt_path(session_name, attempt);
        crate::cartographer::Note::new("runner")
            .squad(&spec.squad_id)
            .cell(&spec.cell_id)
            .task(&spec.task)
            .scope("terminal_log")
            .log_path(&path.to_string_lossy())
            .emit(
                &guard,
                format!("terminal log attempt {attempt} written ({session_name})"),
                serde_json::json!({"session_name": session_name, "attempt": attempt}),
            );
    }

    /// Emit a Cartographer breadcrumb for the tmux auto-reattach retry (see
    /// [`Self::run_via_tmux`]) — deliberately richer than [`Self::emit_tmux_note`]
    /// since this is recovering from an otherwise-silent, still-unexplained
    /// failure mode (`PSMUX_CRASH_NOTES.local.md`). Captures everything
    /// needed to reconstruct "what was lost, what was retried, and why"
    /// after the fact from Cartographer alone, without re-deriving it from
    /// raw pane output.
    #[allow(clippy::too_many_arguments)]
    fn emit_reattach_note(
        &self,
        spec: &RunnerSpec,
        message: &str,
        level: crate::logging::LogLevel,
        attempt: u32,
        resumable_agent_session_id: Option<&str>,
        reason: Option<&str>,
        elapsed_secs: u64,
        error: Option<&str>,
    ) {
        let Some(store) = &self.cartographer else {
            return;
        };
        let Ok(guard) = store.lock() else { return };
        crate::cartographer::Note::new("runner")
            .level(level)
            .squad(&spec.squad_id)
            .cell(&spec.cell_id)
            .task(&spec.task)
            .scope("tmux-reattach")
            .emit(
                &guard,
                message,
                serde_json::json!({
                    "attempt": attempt,
                    "max_attempts": Self::MAX_REATTACH_ATTEMPTS,
                    "agent": spec.agent,
                    "resumable_agent_session_id": resumable_agent_session_id,
                    "reason": reason,
                    "elapsed_secs": elapsed_secs,
                    "error": error,
                }),
            );
    }

    /// Read and parse the `CellResult` a tmux-wrapped runner wrote to
    /// `path`. A missing or malformed file (the runner crashed before
    /// writing it, or the session was killed before it finished) is
    /// reported as a failure, never a panic.
    ///
    /// `last_pane` — the tail of the most recent successful `capture-pane`
    /// read before this call, if any — is folded into a "no result file"
    /// failure so the surfaced error shows what the agent was doing/printing
    /// right before its tmux session disappeared. Without this, a session
    /// that vanishes mid-run (observed on the Windows psmux build this
    /// project targets, root cause not yet pinned down) produces only the
    /// bare OS error and no clue what the agent was actually doing at the
    /// time — nothing to diagnose from if it happens again.
    fn read_tmux_result(path: &std::path::Path, last_pane: Option<&str>) -> RunnerResult {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                RunnerResult::failure(format!("runner result file was not valid JSON: {e}"))
            }),
            Err(e) => {
                // RAL-264: this pane tail becomes the branch `detail` (surfaced
                // by `ralphus review show`/`worktrees` and stored in SQLite), so
                // scrub resolved `from_env` secret values out of it first — the
                // pane can legitimately show the agent's own `$env:... =
                // 'sk-or-v1-...'` assignment and it must not be persisted here.
                //
                // RAL-247: additionally scrub credential env-var values by
                // pattern, so a secret value that was never registered is
                // still caught before this failure message is surfaced.
                let tail = last_pane.map(|pane| {
                    let registered_scrubbed = crate::redact::redact_all(pane);
                    let redacted = ralphus_core::redact::redact_secrets(&registered_scrubbed);
                    tail_lines(&redacted, 60)
                });
                match tail {
                    Some(tail) if !tail.is_empty() => RunnerResult::failure(format!(
                        "runner produced no result file: {e}\nlast pane output:\n{tail}"
                    )),
                    _ => RunnerResult::failure(format!("runner produced no result file: {e}")),
                }
            }
        }
    }
}

/// Parse and persist one `RALPHUS_EVENT:` JSON payload from the runner
/// subprocess. Missing `squad_id`/`cell_id`/`task` fall back to the owning
/// cell's spec. Returns the `agent_session_id` it just persisted, if the
/// event's payload carried one — so a caller tracking a resumable session id
/// locally (see `SubprocessRunner::run_via_tmux`'s auto-reattach retry) can
/// pick it up without a second JSON parse.
pub(crate) fn forward_runner_event(
    cartographer: Option<&Arc<Mutex<Store>>>,
    squad_id: &str,
    cell_id: &str,
    task: &str,
    json: &str,
) -> ForwardedEvent {
    let Some(store) = cartographer else {
        return ForwardedEvent::default();
    };
    let mut event: RunnerEvent = match serde_json::from_str(json) {
        Ok(e) => e,
        Err(e) => {
            let registered = crate::redact::redact_all(json);
            let redacted = ralphus_core::redact::redact_secrets(&registered);
            // ralphus[ignore-rlog-pair]: malformed subprocess output is deliberately kept out of Cartographer -- see the test asserting only well-formed markers are recorded.
            crate::rlog!(
                WARNING,
                "ralphus [runner] malformed RALPHUS_EVENT: {e} ({redacted:?})"
            );
            return ForwardedEvent::default();
        }
    };
    redact_event(&mut event);
    let level = event
        .level
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(crate::logging::LogLevel::INFO);
    let Ok(guard) = store.lock() else {
        return ForwardedEvent::default();
    };
    // RAL-102 follow-up: as soon as the runner reports the agent's session id
    // — the claude-code backend's `stream-json` init event, codex's
    // `thread.started`, pi's equivalent — persist it immediately rather than
    // waiting for the whole cell to finish, so the board's "Open Agent" action
    // activates right away. A no-op for any event whose
    // (squad_id, task, cell_id) isn't a cell row — proof steps and Guardian
    // resolver invocations share this same forwarding path but aren't rows in
    // the `cells` table.
    //
    // Keyed on the *payload* carrying `agent_session_id`, never on the event's
    // `source`: each CLI-agent backend names its own source (`claude-code`,
    // `codex`, `pi`) while the machine providers forward a generic
    // `llm-invoke`, so the payload key is the only thing all of them agree on.
    // A new backend gets this for free by emitting the same key, and renaming
    // a source cannot silently sever the capture.
    let mut captured_agent_session_id = None;
    // RAL-161: likewise, persist live token/cost usage as soon as an event's
    // payload carries `cost_usd`, and hand it back so the tmux poll loop can
    // compare it against the cell's `maximum_budget_usd` cap without a second
    // JSON parse or DB round-trip.
    let mut live_usage = None;
    if let Some(sid) = event
        .payload
        .get("agent_session_id")
        .and_then(|v| v.as_str())
    {
        let _ = guard.set_cell_agent_session_id_live(
            event.squad_id.as_deref().unwrap_or(squad_id),
            event.task.as_deref().unwrap_or(task),
            event.cell_id.as_deref().unwrap_or(cell_id),
            sid,
        );
        captured_agent_session_id = Some(sid.to_string());
    }
    if let Some(cost_usd) = event
        .payload
        .get("cost_usd")
        .and_then(serde_json::Value::as_f64)
    {
        let tokens_in = event
            .payload
            .get("tokens_in")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let tokens_out = event
            .payload
            .get("tokens_out")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        // RAL-326: absent for a backend that reports no cache breakdown, and
        // for any pre-RAL-326 runner still emitting the two-field payload.
        let cache_creation_tokens = event
            .payload
            .get("cache_creation_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let cache_read_tokens = event
            .payload
            .get("cache_read_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let usage = LiveUsage {
            tokens_in,
            tokens_out,
            cache_creation_tokens,
            cache_read_tokens,
            cost_usd,
        };
        let _ = guard.set_cell_live_usage(
            event.squad_id.as_deref().unwrap_or(squad_id),
            event.task.as_deref().unwrap_or(task),
            event.cell_id.as_deref().unwrap_or(cell_id),
            usage,
        );
        live_usage = Some(usage);
    }
    if event.message != LIVE_USAGE_MESSAGE {
        let _ = guard.cartographer_log(crate::cartographer::CartographerEntry {
            level,
            source: &event.source,
            message: &event.message,
            scope: event.scope.as_deref(),
            squad_id: Some(event.squad_id.as_deref().unwrap_or(squad_id)),
            guardian_id: None,
            cell_id: Some(event.cell_id.as_deref().unwrap_or(cell_id)),
            task: Some(event.task.as_deref().unwrap_or(task)),
            log_path: None,
            payload: event.payload,
            admin_only: false,
        });
    }
    ForwardedEvent {
        agent_session_id: captured_agent_session_id,
        live_usage,
    }
}

fn redact_event(event: &mut RunnerEvent) {
    fn text(value: &mut String) {
        let registered = crate::redact::redact_all(value);
        *value = ralphus_core::redact::redact_secrets(&registered).into_owned();
    }
    fn value(node: &mut serde_json::Value) {
        match node {
            serde_json::Value::String(string) => text(string),
            serde_json::Value::Array(items) => items.iter_mut().for_each(value),
            serde_json::Value::Object(fields) => fields.values_mut().for_each(value),
            _ => {}
        }
    }

    text(&mut event.source);
    text(&mut event.message);
    for field in [
        &mut event.level,
        &mut event.scope,
        &mut event.squad_id,
        &mut event.cell_id,
        &mut event.task,
    ]
    .into_iter()
    .flatten()
    {
        text(field);
    }
    value(&mut event.payload);
}

/// Fold the last-known live usage snapshot into a result that reports none
/// of its own (RAL-187).
///
/// `record_cell_result`'s write of `tokens_in`/`tokens_out`/`cost_usd` is
/// a plain overwrite, not a `COALESCE` — so a [`RunnerResult::failure`],
/// which zeroes all three, erases whatever per-turn usage
/// [`Store::set_cell_live_usage`] already persisted while the cell was
/// running. That is not hypothetical: a Codex cell observed in
/// `run-000000000151` ran for minutes, completed several turns, then lost its
/// tmux pane before writing a result file — and was recorded as `0` tokens
/// forever, even though those tokens were genuinely spent. Every mid-run
/// failure path (cancel, timeout, lost pane, unparseable result file) funnels
/// through `failure()`, so backfilling once here covers all of them.
///
/// Only applies when the result reports nothing at all; a completed run's own
/// numbers are authoritative and are never overwritten by a stale snapshot.
/// [`RunnerResult::cost_exceeded`] already carries real figures, so this is a
/// no-op for it.
fn backfill_live_usage(result: &mut RunnerResult, live: LiveUsage) {
    if result.tokens_in == 0
        && result.tokens_out == 0
        && result.cache_creation_tokens == 0
        && result.cache_read_tokens == 0
        && result.cost_usd == 0.0
    {
        result.tokens_in = live.tokens_in;
        result.tokens_out = live.tokens_out;
        result.cache_creation_tokens = live.cache_creation_tokens;
        result.cache_read_tokens = live.cache_read_tokens;
        result.cost_usd = live.cost_usd;
        // RAL-326: what lands here is a mid-run snapshot priced by
        // `claude_code_backend::estimate_cost_usd`'s approximate table, not
        // the backend's own final figure -- and it stops at whatever turn the
        // process died on. Marking it keeps the board from presenting a
        // permanently-recorded guess as an authoritative bill.
        result.cost_is_estimated = true;
    }
}

/// Whether `elapsed` has reached the optional `deadline`. `None` = no limit,
/// so never times out. Factored out so the timeout rule is unit-testable
/// without spawning a real subprocess (RAL-15).
fn timed_out(elapsed: Duration, deadline: Option<Duration>) -> bool {
    matches!(deadline, Some(d) if elapsed >= d)
}

/// The last `n` non-empty lines of `text`, joined back with newlines — used
/// to fold a bit of tmux pane context into a "no result file" failure
/// message. Empty when `text` has no non-empty lines. `pub(crate)` since
/// `tmux.rs::write_pane_snapshot` also truncates a persisted pane snapshot
/// with it.
pub(crate) fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    // Test harness output (`SKIP:` notices) legitimately goes to stdout so
    // `cargo test --nocapture` shows it; no JSON contract exists here.
    #![allow(clippy::print_stdout)]

    use super::*;

    #[test]
    fn pane_shows_done_sentinel_true_for_the_real_wrapper_line() {
        assert!(pane_shows_done_sentinel(
            "some earlier output\nRALPHUS_TMUX_DONE: done\n"
        ));
        assert!(pane_shows_done_sentinel("RALPHUS_TMUX_DONE: failed"));
        // Leading whitespace on the sentinel's own line is still fine.
        assert!(pane_shows_done_sentinel("  RALPHUS_TMUX_DONE: done"));
    }

    /// RAL-1xx: `pane.contains(TMUX_DONE_MARKER)` used to false-positive
    /// whenever the marker text appeared *anywhere* in the pane — including
    /// in a session's own legitimate tool output, not just the real wrapper
    /// line. Observed in production for a live-view/tmux-completion ticket
    /// whose own work involves reading/grepping `runner.rs`/`__main__.py`,
    /// both of which contain the literal string `RALPHUS_TMUX_DONE` in
    /// their own source — the daemon believed the still-actively-working
    /// session had finished, tried to read a result file that didn't exist
    /// yet, and killed the pane out from under real, in-progress work.
    #[test]
    fn pane_shows_done_sentinel_false_for_marker_text_embedded_in_other_output() {
        // A grep match: the marker isn't at the start of the line.
        assert!(!pane_shows_done_sentinel(
            "daemon/src/runner.rs:402:const TMUX_DONE_MARKER: &str = \"RALPHUS_TMUX_DONE\";"
        ));
        // A `Read` tool's numbered listing, same reason.
        assert!(!pane_shows_done_sentinel(
            "402\tconst TMUX_DONE_MARKER: &str = \"RALPHUS_TMUX_DONE\";"
        ));
        // The marker mentioned mid-sentence in the agent's own prose.
        assert!(!pane_shows_done_sentinel(
            "I'll check for the RALPHUS_TMUX_DONE sentinel in the pane."
        ));
        // No sentinel at all.
        assert!(!pane_shows_done_sentinel("still working on it...\n"));
    }

    /// Asserts the structure of the fully assembled unattended-cell system
    /// prompt (caller-authored fragment + ralphus fragments joined with blank
    /// lines), the form the board shows and the backend receives -- not just
    /// the individual constants.
    fn assert_assembled_prompt_sections(sp: &str, caller_first_line: &str) {
        assert!(
            sp.starts_with(caller_first_line),
            "caller-authored fragment must come first: {sp}"
        );
        let background = sp.find("## Background\n").expect("Background section");
        let tools = sp
            .find("## Regarding Tools\n")
            .expect("Regarding Tools section");
        let conclusion = sp.find("## Conclusion\n").expect("Conclusion section");
        assert!(background < tools && tools < conclusion);
        // Tool guidance: prefer rg, allow grep as fallback, example form.
        assert!(sp.contains("Prefer `rg` for shell searches"), "{sp}");
        assert!(
            sp.contains("use `grep` only when `rg` is unavailable"),
            "{sp}"
        );
        assert!(sp.contains("use `rg \"pattern\" .`"), "{sp}");
        // Synchronous-execution constraints with the exact escape marker.
        assert!(
            sp.contains("never launch the thing you are checking as a background"),
            "{sp}"
        );
        assert!(
            sp.contains("'RALPHUS_STILL_WORKING: <one-line reason>'"),
            "{sp}"
        );
    }

    #[test]
    fn assembled_unattended_cell_system_prompt_is_sectioned() {
        let sp = effective_cell_system_prompt(
            Some("Do NOT commit and do NOT push under any circumstances."),
            &[],
        );
        assert_assembled_prompt_sections(
            &sp,
            "Do NOT commit and do NOT push under any circumstances.",
        );
        // Ghost handoff contract with the exact marker and 5-bullet cap.
        assert!(
            sp.contains("exact marker 'RALPHUS_GHOST:', followed by up to 5 short"),
            "{sp}"
        );
        assert!(sp.contains("'RALPHUS_GHOST: (nothing to report)'"), "{sp}");
        assert!(!sp.contains("RALPHUS_PROOF:"), "{sp}");
        // No human follow-up, but Ralphus itself may re-invoke synchronously.
        assert!(sp.contains("no human will check back on you"), "{sp}");
        assert!(
            sp.contains("Ralphus may re-invoke you synchronously"),
            "{sp}"
        );
    }

    #[test]
    fn assembled_proof_system_prompt_is_sectioned() {
        let sp = effective_proof_system_prompt(Some(
            "Do NOT commit and do NOT push under any circumstances.",
        ));
        assert_assembled_prompt_sections(
            &sp,
            "Do NOT commit and do NOT push under any circumstances.",
        );
        // Proof steps get the verdict contract, not the ghost handoff.
        assert!(sp.contains("RALPHUS_PROOF: PASS"), "{sp}");
        assert!(sp.contains("RALPHUS_PROOF: FAIL"), "{sp}");
        assert!(!sp.contains("RALPHUS_GHOST:"), "{sp}");
    }

    #[test]
    fn spec_serializes_expected_fields() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("cargo build".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"squad_id\":\"run-1\""));
        assert!(json.contains("\"command\":\"cargo build\""));
        assert!(json.contains("\"cwd\":\"/repo\""));
    }

    /// RAL-303: `from_row` must actually resolve and forward
    /// `[live_view] tool_arg_truncate_chars` -- catches a regression where
    /// the field is added to the wire struct but never populated, which
    /// would silently leave the runner on its own hardcoded fallback forever.
    #[test]
    fn spec_from_row_resolves_tool_arg_truncate_chars() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude-code".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        // No `[live_view]` config file in the test environment, so this
        // exercises the resolved default (200) rather than `None` --
        // `from_row` must always resolve *some* value, never leave the
        // field unset for a normal cell dispatch.
        assert_eq!(spec.tool_arg_truncate_chars, Some(200));
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"tool_arg_truncate_chars\":200"));
    }

    #[test]
    fn spec_carries_maximum_context_and_auto_compact_threshold_from_row() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude-code".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: Some(100_000),
            auto_compact_threshold: Some(80_000),
            maximum_tool_output_tokens: Some(40_000),
            upstream: None,
            machine: None,
            share_session: false,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        assert_eq!(spec.maximum_context, Some(100_000));
        assert_eq!(spec.auto_compact_threshold, Some(80_000));
        assert_eq!(spec.maximum_tool_output_tokens, Some(40_000));
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"maximum_context\":100000"));
        assert!(json.contains("\"auto_compact_threshold\":80000"));
        assert!(json.contains("\"maximum_tool_output_tokens\":40000"));
    }

    #[test]
    fn spec_carries_system_prompt_from_row() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude-code".to_string(),
            model: None,
            system_prompt: Some("Follow the house style.".to_string()),
            system_prompt_position: Some("append".to_string()),
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        assert_eq!(
            spec.system_prompt.as_deref(),
            Some("Follow the house style.")
        );
        assert_eq!(spec.system_prompt_position.as_deref(), Some("append"));
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"system_prompt\":\"Follow the house style.\""));
        assert!(json.contains("\"system_prompt_position\":\"append\""));
    }

    /// RAL-241: a minimal spec for exercising `check_stall_escalation`
    /// directly — no tmux/subprocess involved, since that method only reads
    /// `spec`'s entity fields and talks to the attached `Store`.
    fn stall_test_spec() -> RunnerSpec {
        RunnerSpec {
            squad_id: "squad-000000000001".to_string(),
            task: "build".to_string(),
            cell_id: "cell-1".to_string(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            prompt: Some("do something".to_string()),
            command: None,
            agent: "claude-code".to_string(),
            executable: None,
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            proof: false,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            env_overrides: BTreeMap::new(),
            machine: None,
            tool_arg_truncate_chars: None,
            thrash_max_compactions: None,
            thrash_min_turn_gap: None,
            allow_personal_settings: false,
            allow_personal_memory: false,
        }
    }

    /// Inserts a minimal `squads` row so `mailbox_messages.squad_id`'s
    /// foreign key (enqueued by `check_stall_escalation`) is satisfiable.
    fn insert_squad_for_stall_test(store: &Store, id: &str) {
        store
            .conn
            .execute(
                "INSERT INTO squads(id, state, created_at_ms, updated_at_ms) VALUES(?1, 'running', 0, 0)",
                rusqlite::params![id],
            )
            .unwrap();
    }

    #[test]
    fn check_stall_escalation_fires_once_past_threshold_and_not_before() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        insert_squad_for_stall_test(&store.lock().unwrap(), "squad-000000000001");
        let runner = SubprocessRunner::new("unused").with_cartographer(Arc::clone(&store));
        let spec = stall_test_spec();
        let session_name = "ralphus_test_stall_session";
        let attempt_started_ms = crate::store::now_ms();
        let zero_threshold = Duration::from_secs(0);

        // Threshold is 0s, so the very first check already qualifies as
        // "stalled" -- but must only enqueue once, not on every poll.
        runner.check_stall_escalation(&spec, session_name, attempt_started_ms, zero_threshold);
        runner.check_stall_escalation(&spec, session_name, attempt_started_ms, zero_threshold);
        runner.check_stall_escalation(&spec, session_name, attempt_started_ms, zero_threshold);

        let client_id = store.lock().unwrap().register_mailbox_client().unwrap();
        let messages = store
            .lock()
            .unwrap()
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(
            messages.len(),
            1,
            "an ongoing stall must not re-enqueue on every poll"
        );
        assert_eq!(messages[0].priority, "high");
        assert!(messages[0].message.contains("stalled"));

        // Fresh activity, then a *new* stall onset, must escalate again.
        // `is_stall_escalated` dedupes by exact `last_activity_ms`, so this
        // must be a value that provably differs from the first escalation's
        // (which fell back to `attempt_started_ms`) -- two real `now_ms()`
        // calls a few lines apart can land in the same millisecond on a fast
        // CI runner and collide, making this a flaky no-op instead of a new
        // escalation. `attempt_started_ms - 1` is guaranteed both distinct
        // and no later than any subsequent `now_ms()` reading, so the
        // elapsed-since-activity check below reliably clears the (zero)
        // threshold instead.
        let fresh_activity_ms = attempt_started_ms - 1;
        store
            .lock()
            .unwrap()
            .note_live_activity(session_name, fresh_activity_ms);
        runner.check_stall_escalation(&spec, session_name, attempt_started_ms, zero_threshold);
        let messages_after = store
            .lock()
            .unwrap()
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert_eq!(
            messages_after.len(),
            2,
            "a fresh activity burst followed by a new stall must escalate again"
        );
    }

    #[test]
    fn check_stall_escalation_is_a_noop_below_threshold() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let runner = SubprocessRunner::new("unused").with_cartographer(Arc::clone(&store));
        let spec = stall_test_spec();
        let attempt_started_ms = crate::store::now_ms();

        runner.check_stall_escalation(
            &spec,
            "ralphus_test_no_stall",
            attempt_started_ms,
            Duration::from_secs(600),
        );

        let client_id = store.lock().unwrap().register_mailbox_client().unwrap();
        let messages = store
            .lock()
            .unwrap()
            .mailbox_messages_for_client(&client_id, true, None)
            .unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn prompt_cell_effective_system_prompt_includes_ralphus_defaults() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude-code".to_string(),
            model: None,
            system_prompt: Some("Follow the house style.".to_string()),
            system_prompt_position: Some("append".to_string()),
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let effective = RunnerSpec::from_row("run-1", &row)
            .effective_system_prompt()
            .expect("prompt cells should have a system prompt");
        assert!(effective.contains("Follow the house style."));
        assert!(effective.contains("non-interactive cell"));
        assert!(effective.contains("single, non-interactive invocation"));
        assert!(effective.contains("RALPHUS_GHOST:"));
        assert!(!effective.contains("RALPHUS_PROOF: PASS"));
    }

    #[test]
    fn prompt_proof_effective_system_prompt_uses_proof_instructions_not_ghost() {
        let effective = RunnerSpec::for_proof(
            "run-1",
            "build",
            "verify-session-0",
            "/repo",
            "check something",
            "ollama",
            Some("qwen3:8b"),
            Some(300),
            Some(10000),
            None,
        )
        .effective_system_prompt()
        .expect("prompt proofs should have a system prompt");
        assert!(effective.contains("PROOF step"));
        assert!(effective.contains("RALPHUS_PROOF: PASS"));
        assert!(effective.contains("RALPHUS_PROOF: FAIL"));
        assert!(!effective.contains("RALPHUS_GHOST:"));
    }

    #[test]
    fn command_specs_have_no_effective_system_prompt() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("cargo build".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: Some("unused".to_string()),
            system_prompt_position: Some("append".to_string()),
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        assert!(
            RunnerSpec::from_row("run-1", &row)
                .effective_system_prompt()
                .is_none()
        );
    }

    #[test]
    fn spec_omits_system_prompt_when_unset() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "build".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("cargo build".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let json = serde_json::to_string(&RunnerSpec::from_row("run-1", &row)).unwrap();
        assert!(!json.contains("system_prompt"));
    }

    #[test]
    fn for_proof_builds_a_prompt_spec_with_proof_set() {
        let spec = RunnerSpec::for_proof(
            "run-1",
            "build",
            "verify-session-0",
            "/repo",
            "check something",
            "ollama",
            Some("qwen3:8b"),
            Some(300),
            Some(10000),
            None,
        );
        assert!(spec.proof);
        assert_eq!(spec.timeout_sec, Some(300));
        assert_eq!(spec.budget_tokens, Some(10000));
        assert_eq!(spec.command, None);
        assert_eq!(spec.prompt.as_deref(), Some("check something"));
        assert_eq!(spec.agent, "ollama");
        assert_eq!(spec.model.as_deref(), Some("qwen3:8b"));
    }

    #[test]
    fn proof_passed_requires_done_and_true_verdict() {
        let mut r = RunnerResult {
            status: "done".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            compaction_input_tokens: 0,
            compaction_count: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: String::new(),
            error: None,
            proofed: Some(true),
            agent_session_id: None,
            ghost: None,
        };
        assert!(r.proof_passed());

        r.proofed = Some(false);
        assert!(!r.proof_passed());

        r.proofed = None;
        assert!(!r.proof_passed(), "no verdict should fail closed");

        r.status = "failed".to_string();
        r.proofed = Some(true);
        assert!(
            !r.proof_passed(),
            "a crashed proof can't have passed even with a stray verdict"
        );
    }

    #[test]
    fn subprojects_single_injects_system_prompt() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/mono".to_string()),
            subprojects: vec!["packages/foo".to_string()],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "ollama".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        let sp = spec
            .system_prompt
            .as_deref()
            .expect("system_prompt should be injected");
        assert!(
            sp.contains("packages/foo"),
            "addendum must name the subproject: {sp}"
        );
        assert!(
            sp.contains("monorepo"),
            "addendum should mention monorepo: {sp}"
        );
        assert_eq!(spec.system_prompt_position.as_deref(), Some("append"));
    }

    #[test]
    fn subprojects_multiple_all_listed() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/mono".to_string()),
            subprojects: vec!["packages/alpha".to_string(), "packages/beta".to_string()],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "ollama".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        let sp = spec
            .system_prompt
            .as_deref()
            .expect("system_prompt injected");
        assert!(sp.contains("packages/alpha"), "alpha in addendum: {sp}");
        assert!(sp.contains("packages/beta"), "beta in addendum: {sp}");
        assert_eq!(spec.system_prompt_position.as_deref(), Some("append"));
    }

    #[test]
    fn subprojects_merges_with_existing_system_prompt() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/mono".to_string()),
            subprojects: vec!["services/auth".to_string()],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "claude-code".to_string(),
            model: None,
            system_prompt: Some("Follow the style guide.".to_string()),
            system_prompt_position: Some("append".to_string()),
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        let sp = spec
            .system_prompt
            .as_deref()
            .expect("system_prompt should be set");
        assert!(
            sp.contains("Follow the style guide."),
            "user prompt preserved: {sp}"
        );
        assert!(
            sp.contains("services/auth"),
            "subprojects addendum appended: {sp}"
        );
        assert_eq!(spec.system_prompt_position.as_deref(), Some("append"));
    }

    #[test]
    fn no_subproject_preserves_nil_system_prompt() {
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some("/repo".to_string()),
            subprojects: vec![],
            prompt: Some("do work".to_string()),
            command: None,
            agent: "ollama".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            timeout_sec: None,
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let spec = RunnerSpec::from_row("run-1", &row);
        assert!(
            spec.system_prompt.is_none(),
            "no injection when subproject is absent"
        );
        assert!(spec.system_prompt_position.is_none());
    }

    #[test]
    fn forward_runner_event_persists_a_well_formed_marker_and_skips_malformed_ones() {
        // `forward_runner_event` is what both the tmux pane poll loop
        // (`run_via_tmux_attempt`) and the reattach path call for each
        // `RALPHUS_EVENT:` line they see — exercised directly here rather
        // than via a raw stderr pipe reader, which no longer exists now that
        // every spec (prompt and command alike) runs tmux-wrapped (RAL-151).
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        forward_runner_event(
            Some(&store),
            "run-1",
            "s0",
            "build",
            "{\"source\":\"llm\",\"message\":\"session start\",\"level\":\"info\",\"scope\":\"session\",\"payload\":{\"prompt_len\":42}}",
        );
        // A malformed marker payload must not be recorded.
        forward_runner_event(Some(&store), "run-1", "s0", "build", "not json at all");

        let page = store
            .lock()
            .unwrap()
            .cartographer_query(&crate::cartographer::CartographerFilter::recent(10))
            .unwrap();
        assert_eq!(
            page.total, 1,
            "the malformed marker line must not be recorded"
        );
        let row = &page.rows[0];
        assert_eq!(row.source, "llm");
        assert_eq!(row.message, "session start");
        assert_eq!(row.scope.as_deref(), Some("session"));
        assert_eq!(row.squad_id.as_deref(), Some("run-1"));
        assert_eq!(row.cell_id.as_deref(), Some("s0"));
        assert_eq!(row.payload, serde_json::json!({"prompt_len": 42}));
    }

    #[test]
    fn forward_runner_event_redacts_registered_secrets_before_cartographer() {
        crate::redact::with_registry_lock(|| {
            crate::redact::clear_for_tests();
            crate::redact::register("phase7-event-secret");
            let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
            forward_runner_event(
                Some(&store),
                "run-1",
                "s0",
                "build",
                &serde_json::json!({
                    "source": "remote-runner",
                    "message": "saw phase7-event-secret",
                    "payload": {
                        "nested": ["phase7-event-secret", {"detail": "x phase7-event-secret y"}]
                    }
                })
                .to_string(),
            );
            let page = store
                .lock()
                .unwrap()
                .cartographer_query(&crate::cartographer::CartographerFilter::recent(10))
                .unwrap();
            let rendered = format!("{:?}", page.rows);
            assert!(!rendered.contains("phase7-event-secret"), "{rendered}");
            assert!(rendered.contains(crate::redact::REDACTED), "{rendered}");
            crate::redact::clear_for_tests();
        });
    }

    /// A task file with one `build` task holding one `worker` cell — the
    /// (task, cell_id) pair the live-capture tests below address their events
    /// to, since `Store::set_cell_agent_session_id_live` matches on both.
    const LIVE_CAPTURE_SAMPLE: &str = r#"
[[task]]
name = "build"
[[task.cell]]
id = "worker"
cwd = "/repo"
prompt = "make it build"
"#;

    fn store_with_one_cell() -> (Arc<Mutex<Store>>, String) {
        let mut store = Store::open_in_memory().unwrap();
        let file: ralphus_core::schema::TaskFile =
            toml::from_str(LIVE_CAPTURE_SAMPLE).expect("valid toml");
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        (Arc::new(Mutex::new(store)), squad_id)
    }

    /// Every CLI-agent backend names its own event source (`claude-code`,
    /// `codex`, `pi`) while the machine providers forward a generic
    /// `llm-invoke`. The capture keys on the payload rather than the source so
    /// all four land, and so renaming a backend cannot silently sever it —
    /// without this, "Open Agent" stays disabled for the whole life of a
    /// running cell and `resumable_agent_session_id` never reaches the tmux
    /// auto-reattach retry.
    #[test]
    fn forward_runner_event_captures_session_id_from_every_backend_source() {
        for (i, source) in ["claude-code", "codex", "pi", "llm-invoke"]
            .into_iter()
            .enumerate()
        {
            let (store, squad_id) = store_with_one_cell();
            let sid = format!("sid-{i}");
            let json = serde_json::json!({
                "source": source,
                "message": "session-id known",
                "level": "info",
                "payload": {"agent_session_id": sid},
            })
            .to_string();

            let fwd = forward_runner_event(Some(&store), &squad_id, "worker", "build", &json);

            assert_eq!(
                fwd.agent_session_id.as_deref(),
                Some(sid.as_str()),
                "{source}: the id must be handed back for the reattach retry"
            );
            let (_, _, persisted) = store
                .lock()
                .unwrap()
                .get_cell_agent_resume(&squad_id, 0, 0)
                .unwrap();
            assert_eq!(
                persisted.as_deref(),
                Some(sid.as_str()),
                "{source}: the id must be persisted while the cell is still running"
            );
        }
    }

    /// The live-usage half of the same capture (RAL-161). The returned
    /// snapshot is what `run_via_tmux_attempt`'s cost-cap check compares
    /// against `maximum_budget_usd`, so a miss here silently disables every
    /// configured cost cap.
    #[test]
    fn forward_runner_event_captures_live_usage_from_every_backend_source() {
        for (i, source) in ["claude-code", "codex", "pi", "llm-invoke"]
            .into_iter()
            .enumerate()
        {
            let (store, squad_id) = store_with_one_cell();
            let tokens_in = i64::try_from(i).unwrap() + 1;
            let json = serde_json::json!({
                "source": source,
                "message": "live usage",
                "level": "info",
                "payload": {
                    "tokens_in": tokens_in,
                    "tokens_out": 7,
                    "cache_creation_tokens": 11,
                    "cache_read_tokens": 13,
                    "cost_usd": 1.25,
                },
            })
            .to_string();

            let fwd = forward_runner_event(Some(&store), &squad_id, "worker", "build", &json);

            assert_eq!(
                fwd.live_usage,
                Some(LiveUsage {
                    tokens_in,
                    tokens_out: 7,
                    cache_creation_tokens: 11,
                    cache_read_tokens: 13,
                    cost_usd: 1.25,
                }),
                "{source}: the snapshot the cost cap reads must be returned"
            );
            let squad = store.lock().unwrap().get_squad(&squad_id).unwrap();
            let cell = &squad.tasks[0].cells[0];
            assert_eq!(cell.tokens_in, tokens_in, "{source}: tokens_in persisted");
            assert_eq!(cell.tokens_out, 7, "{source}: tokens_out persisted");
            assert_eq!(
                (cell.cache_creation_tokens, cell.cache_read_tokens),
                (11, 13),
                "{source}: RAL-326 cache tokens persisted"
            );
            assert!(
                (cell.cost_usd - 1.25).abs() < f64::EPSILON,
                "{source}: cost_usd persisted, got {}",
                cell.cost_usd
            );
        }
    }

    /// ...and does so *without* leaving a Cartographer row behind: a backend
    /// emits one of these per assistant turn, so persisting them drowns
    /// `ralphus squad timeline` in `INFO <backend> live usage` lines that say
    /// nothing the cell's own usage columns (asserted above) don't already.
    #[test]
    fn forward_runner_event_does_not_persist_a_live_usage_row() {
        let (store, squad_id) = store_with_one_cell();
        let usage = serde_json::json!({
            "source": "claude-code",
            "message": LIVE_USAGE_MESSAGE,
            "level": "info",
            "payload": {"tokens_in": 3, "tokens_out": 7, "cost_usd": 1.25},
        })
        .to_string();

        for _ in 0..5 {
            forward_runner_event(Some(&store), &squad_id, "worker", "build", &usage);
        }
        // A usage payload riding along with a *different* message is a real
        // event and must still be recorded — only the heartbeat is dropped.
        forward_runner_event(
            Some(&store),
            &squad_id,
            "worker",
            "build",
            &serde_json::json!({
                "source": "claude-code",
                "message": "cell finished",
                "level": "info",
                "payload": {"tokens_in": 3, "tokens_out": 7, "cost_usd": 1.25},
            })
            .to_string(),
        );

        let page = store
            .lock()
            .unwrap()
            .cartographer_query(&crate::cartographer::CartographerFilter::recent(50))
            .unwrap();
        let messages: Vec<&str> = page.rows.iter().map(|r| r.message.as_str()).collect();
        assert!(
            !messages.contains(&LIVE_USAGE_MESSAGE),
            "the per-turn usage heartbeat must not reach Cartographer: {messages:?}"
        );
        assert!(
            messages.contains(&"cell finished"),
            "a real event carrying usage must still be recorded: {messages:?}"
        );
    }

    /// An event carrying neither key is still logged to Cartographer but must
    /// not fabricate a session id or a zeroed usage snapshot — a `Some(0.0)`
    /// cost would read as a real reading and reset the cap's running total.
    #[test]
    fn forward_runner_event_reports_nothing_for_an_ordinary_event() {
        let (store, squad_id) = store_with_one_cell();
        let json = serde_json::json!({
            "source": "claude-code",
            "message": "some diagnostic",
            "level": "debug",
            "payload": {"note": "no ids here"},
        })
        .to_string();

        let fwd = forward_runner_event(Some(&store), &squad_id, "worker", "build", &json);

        assert_eq!(fwd, ForwardedEvent::default());
    }

    #[test]
    fn forward_runner_event_without_cartographer_handle_is_a_noop() {
        // Must not panic when no store handle is attached (e.g. test fakes).
        assert_eq!(
            forward_runner_event(
                None,
                "run-1",
                "s0",
                "build",
                "{\"source\":\"llm\",\"message\":\"hi\"}",
            ),
            ForwardedEvent::default()
        );
    }

    #[test]
    fn generate_agent_session_id_looks_like_a_uuid_v4() {
        let id = generate_agent_session_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "must be 8-4-4-4-12 hex groups, got {id:?}"
        );
        assert!(
            parts
                .iter()
                .all(|p| p.chars().all(|c| c.is_ascii_hexdigit())),
            "every group must be hex, got {id:?}"
        );
        assert_eq!(
            parts[2].chars().next(),
            Some('4'),
            "RFC 4122 version nibble must be 4, got {id:?}"
        );
        assert!(
            matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
            "RFC 4122 variant nibble must be 8/9/a/b, got {id:?}"
        );
    }

    #[test]
    fn generate_agent_session_id_is_not_reused() {
        let a = generate_agent_session_id();
        let b = generate_agent_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn tail_lines_keeps_only_the_last_n_non_empty_lines() {
        let text = "a\n\nb\nc\nd\n";
        assert_eq!(tail_lines(text, 2), "c\nd");
        assert_eq!(tail_lines(text, 10), "a\nb\nc\nd");
        assert_eq!(tail_lines("", 5), "");
        assert_eq!(tail_lines("\n\n", 5), "");
    }

    /// RAL-264: a "no result file" failure folds the last pane tail into the
    /// message, and that message becomes the branch `detail` (surfaced by
    /// `ralphus review show`/`worktrees` and persisted to SQLite). When the
    /// pane shows the agent's own `$env:... = 'sk-or-v1-...'` assignment — the
    /// exact leak this ticket exists for — the resolved secret must be
    /// scrubbed out of the failure message before it is returned.
    ///
    /// Exercises the true leak site (`read_tmux_result`) directly: a write-time
    /// reproduction of the incident, where a `from_env`-sourced token value
    /// landed in `last_pane` while the session vanished without writing a
    /// result file.
    #[test]
    fn read_tmux_result_redacts_secret_values_from_folded_pane_text() {
        crate::redact::with_registry_lock(|| {
            crate::redact::clear_for_tests();
            crate::redact::register("sk-or-v1-ral264-unit-test-token");

            let missing = std::env::temp_dir().join(format!(
                "ralphus-test-missing-result-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time")
                    .as_nanos()
            ));
            let _ = std::fs::remove_file(&missing);

            let pane = concat!(
                "some prior work output\n",
                "$env:ANTHROPIC_AUTH_TOKEN = 'sk-or-v1-ral264-unit-test-token'\n",
                "still printing...\n",
            );
            let result = <SubprocessRunner>::read_tmux_result(&missing, Some(pane));
            // The failure is surfaced (there was no result file) and the secret
            // value is gone from the `detail`, replaced by the placeholder.
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .contains("no result file"),
                "expected a no-result-file failure, got {:?}",
                result.error
            );
            let detail = result.error.unwrap_or_default();
            assert!(
                !detail.contains("sk-or-v1-ral264-unit-test-token"),
                "secret leaked into failure detail: {detail}"
            );
            assert!(
                detail.contains(crate::redact::REDACTED),
                "expected the redaction placeholder in detail: {detail}"
            );
            let _ = std::fs::remove_file(&missing);
        });
    }

    #[test]
    fn command_line_splits_program_and_args() {
        let r = SubprocessRunner::new("python -m ralphus.runner");
        assert_eq!(r.program, "python");
        assert_eq!(r.args, vec!["-m", "ralphus.runner"]);
    }

    #[test]
    fn timed_out_respects_the_deadline() {
        // No deadline: never times out.
        assert!(!timed_out(Duration::from_secs(9999), None));
        // Below the deadline: still running.
        assert!(!timed_out(
            Duration::from_secs(29),
            Some(Duration::from_secs(30))
        ));
        // At or past the deadline: timed out.
        assert!(timed_out(
            Duration::from_secs(30),
            Some(Duration::from_secs(30))
        ));
        assert!(timed_out(
            Duration::from_secs(31),
            Some(Duration::from_secs(30))
        ));
    }

    #[test]
    fn backfill_live_usage_rescues_tokens_from_a_failed_attempt() {
        // RAL-187: the real case from `run-000000000151` — a Codex session
        // completed several turns (so live usage was already persisted
        // mid-run), then lost its tmux pane before writing a result file.
        // Without the backfill, the zeroed failure result overwrites those
        // tokens and the board reports 0 forever.
        let live = LiveUsage {
            tokens_in: 8_685_138,
            tokens_out: 49_415,
            cache_creation_tokens: 320_114,
            cache_read_tokens: 7_204_990,
            cost_usd: 0.0,
        };
        let mut lost = RunnerResult::failure("runner produced no result file");
        backfill_live_usage(&mut lost, live);
        assert_eq!(lost.tokens_in, 8_685_138);
        assert_eq!(lost.tokens_out, 49_415);
        assert_eq!(lost.cache_creation_tokens, 320_114);
        assert_eq!(lost.cache_read_tokens, 7_204_990);
        // Codex reports no cost anywhere, so this stays 0 — rendered "N/A".
        assert_eq!(lost.cost_usd, 0.0);
        // RAL-326: a backfilled snapshot is never the backend's own final
        // accounting, so it must carry the estimate marker the board badges.
        assert!(lost.cost_is_estimated);
    }

    #[test]
    fn backfill_live_usage_never_overwrites_a_results_own_figures() {
        let live = LiveUsage {
            tokens_in: 10,
            tokens_out: 2,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.5,
        };
        // A completed run's own numbers are authoritative, even though the
        // stale snapshot happens to be larger.
        let mut done = RunnerResult {
            tokens_in: 7,
            tokens_out: 1,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.25,
            cost_is_estimated: false,
            ..RunnerResult::failure("x")
        };
        backfill_live_usage(&mut done, live);
        assert_eq!(
            (done.tokens_in, done.tokens_out, done.cost_usd),
            (7, 1, 0.25)
        );
        // `cost_exceeded` already carries real figures: a no-op.
        let mut capped = RunnerResult::cost_exceeded(
            LiveUsage {
                tokens_in: 99,
                tokens_out: 9,
                cache_creation_tokens: 4,
                cache_read_tokens: 5,
                cost_usd: 1.5,
            },
            1.0,
        );
        backfill_live_usage(&mut capped, live);
        assert_eq!(
            (
                capped.tokens_in,
                capped.tokens_out,
                capped.cache_creation_tokens,
                capped.cache_read_tokens,
                capped.cost_usd
            ),
            (99, 9, 4, 5, 1.5)
        );
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_registers_pid_while_the_subprocess_is_alive_and_clears_it_after() {
        // A real, briefly-lived subprocess: the runner must register its PID for
        // the resource view while it runs, and drop it once it exits (RAL-11).
        //
        // Every spec now runs tmux-wrapped (RAL-151), so this is a real
        // live-tmux test — serialized under `LIVE_TMUX_TEST_LOCK` and given a
        // per-invocation-unique run_id (RAL-177) for the same reason every
        // other live-tmux test below is: a real tmux session name is
        // machine-wide shared state, and a hardcoded literal would collide
        // with an identical test running concurrently in a sibling worktree.
        //
        // The pane's command (a bare `powershell`/`sleep` call) never speaks
        // the runner result-file/`RALPHUS_TMUX_DONE` protocol real
        // `ralphus-runner` invocations do, so the tmux session itself outlives
        // it (`remain-on-exit`) — a short `timeout_sec` is what eventually
        // ends the attempt, not a natural "done". That's fine: RAL-11's PID
        // registration/deregistration is scoped to the whole tmux-wrapped
        // attempt's lifetime (`find_server_pid`, tied to the session, not the
        // inner command), not to a successful "done" outcome specifically.
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-x");
        let _cleanup =
            crate::tmux::KillSessionOnDrop(crate::tmux::session_name(&run_id, "t", "s0"));
        let reg = crate::procreg::ProcRegistry::new();
        #[cfg(target_os = "windows")]
        let runner = SubprocessRunner {
            program: "powershell".to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Start-Sleep -Milliseconds 700".to_string(),
            ],
            registry: Some(reg.clone()),
            cartographer: None,
            detachments: None,
        };
        #[cfg(not(target_os = "windows"))]
        let runner = SubprocessRunner {
            program: "sleep".to_string(),
            args: vec!["1".to_string()],
            registry: Some(reg.clone()),
            cartographer: None,
            detachments: None,
        };
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            cell_id: "s0".to_string(),
            cwd: Some(".".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("noop".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            // Only a backstop now: the test cancels as soon as it has
            // observed the PID, so this is never normally reached. It still
            // has to stay above `PID_POLL_BUDGET`, or a slow-starting session
            // would race its own timeout-kill (which clears the registered
            // PID) against the poll loop's first chance to see it — the
            // original RAL-171/RAL-177 failure signature. Earlier revisions
            // balanced these two numbers against each other (20s/10s); with
            // cancel-on-observe the balance no longer costs runtime, so the
            // margin is simply generous.
            timeout_sec: Some(120),
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let spec = RunnerSpec::from_row(&run_id, &row);
        // See the sibling `..._for_a_command_kind_spec` test: cancel on
        // observation so runtime tracks startup, not `timeout_sec`.
        let cancel = CancelToken::new();
        let cancel_for_worker = cancel.clone();
        let worker = std::thread::spawn(move || runner.run_cancellable(&spec, &cancel_for_worker));

        // While the session is alive the PID is registered. Windows-only, per
        // `find_server_pid`'s own doc comment — see
        // `live_tmux_registers_and_clears_pid_for_a_command_kind_spec` for
        // the same platform-conditional pattern.
        if cfg!(target_os = "windows") {
            let seen = await_registered_pid(&reg, &run_id, "s0", &worker, PID_POLL_BUDGET);
            assert!(seen.is_some(), "PID should be registered during the run");
        } else {
            std::thread::sleep(Duration::from_millis(300));
        }
        cancel.cancel();

        let _ = worker.join();
        assert_eq!(
            reg.pid_of(&run_id, "s0"),
            None,
            "PID should be cleared once the tmux-wrapped session ends"
        );
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_missing_program_fails_gracefully() {
        // Every spec runs tmux-wrapped since RAL-151, so this is a real
        // live-tmux test — serialized under `LIVE_TMUX_TEST_LOCK` and given a
        // per-invocation-unique run_id (RAL-177), same as every other
        // live-tmux test in this module. An unresolvable runner program is
        // typed into the pane via `send-keys`/`respawn-pane` rather than
        // spawned directly, so it now surfaces as a bounded timeout (the
        // pane just shows a shell "command not found" error) rather than an
        // immediate "could not spawn" — see
        // `live_tmux_missing_program_times_out_instead_of_spawn_error` below
        // for the identical, already-established pattern this mirrors.
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-1");
        let _cleanup = crate::tmux::KillSessionOnDrop(crate::tmux::session_name(&run_id, "t", "s"));
        let runner = SubprocessRunner::new("definitely-not-a-real-program-xyz");
        let row = CellRow {
            task_idx: 0,
            idx: 0,
            task_name: "t".to_string(),
            cell_id: "s".to_string(),
            cwd: Some(".".to_string()),
            subprojects: vec![],
            prompt: None,
            command: Some("echo hi".to_string()),
            agent: "claude".to_string(),
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            depends_on: vec![],
            // Every spec runs tmux-wrapped (RAL-151), so a nonexistent
            // program doesn't fail spawn itself — the shell inside the pane
            // reports "not recognized" and stays open (no done sentinel is
            // ever printed). Without a bound, the runner would only give up
            // once the underlying tmux server's own idle-exit kicks in,
            // which is both slow and non-deterministic. Bound it here so the
            // test fails via the runner's own timeout path instead. Kept at
            // 1s (rather than a more generous bound) to prevent concurrent
            // tests from breaking each other.
            timeout_sec: Some(1),
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            upstream: None,
            machine: None,
            share_session: false,
        };
        let result = runner.run(&RunnerSpec::from_row(&run_id, &row));
        assert!(!result.is_done());
        let err = result.error.unwrap();
        assert!(
            err.contains("timed out"),
            "expected a timeout error, got: {err}"
        );
    }

    // ── tmux-wrapped path (RAL-102, and RAL-151 for `command`-kind specs) ──
    //
    // These exercise `run_via_tmux` against the real `tmux` binary with a
    // tiny fake "runner" program (a one-line Python script) standing in for
    // `ralphus-runner`, so no dependency on pydantic-ai/Claude is needed.
    // Skips (rather than fails) when `tmux` or `python` isn't on PATH,
    // mirroring the project's live-Ollama-integration-test skip idiom.

    fn tmux_and_python_available() -> bool {
        let has_tmux = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| {
                dir.join("tmux").is_file()
                    || dir.join("tmux.exe").is_file()
                    || dir.join("tmux.cmd").is_file()
            })
        });
        let has_python = std::process::Command::new("python")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        has_tmux && has_python
    }

    /// A fake `ralphus-runner`: reads no real spec, just writes a canned
    /// `CellResult` to whatever `--result-file` path it was given and
    /// prints the `RALPHUS_TMUX_DONE` sentinel — mirroring
    /// `cli/src/ralphus/runner/__main__.py`'s `--result-file` contract
    /// exactly (RAL-102).
    // A single logical line (no embedded newlines): the runner's command is
    // delivered by typing it into an interactive pane via `send-keys` on
    // Windows, and an embedded newline there is indistinguishable from a
    // literal Enter keypress, splitting the command into separate lines the
    // shell executes independently instead of one `python -c ...` call.
    //
    // The sentinel string is built via concatenation (`"RALPHUS_TMUX" +
    // "_DONE: done"`) rather than written out whole: `capture-pane` sees the
    // *typed command itself* (this script's own source) echoed into the pane
    // before it ever runs, so a literal `RALPHUS_TMUX_DONE` in the source
    // would false-positive the sentinel scan on the echoed input, not just
    // the eventual real output. Real `ralphus-runner` invocations never hit
    // this — their command line is just `<program> <args>`, never Python
    // source containing the marker — so this is a test-fake-only concern.
    const FAKE_RUNNER_SCRIPT: &str = "import sys,json; \
        rp=sys.argv[sys.argv.index(\"--result-file\")+1]; \
        open(rp,\"w\").write(json.dumps({\"status\":\"done\",\"tokens_in\":1,\"tokens_out\":2,\"cost_usd\":0.01,\"summary\":\"fake\",\"error\":None,\"proofed\":None,\"agent_session_id\":None})); \
        print(\"RALPHUS_TMUX\" + \"_DONE: done\")";

    /// A fake runner that never finishes, to exercise the timeout path.
    const HANGING_RUNNER_SCRIPT: &str = "import time; time.sleep(30)";

    /// Wall-clock budget the PID-registration tests allow for a tmux-wrapped
    /// session to start and become visible in the process table.
    ///
    /// Generous on purpose: it is only ever fully consumed when the test is
    /// about to fail anyway (see [`await_registered_pid`], which returns as
    /// soon as the PID appears *or* the session dies). The predecessor was an
    /// iteration count of 100 × 100 ms — a hard 10 s ceiling that a machine
    /// running the rest of the suite in parallel could genuinely exceed just
    /// on tmux-server + Python startup, producing a flake indistinguishable
    /// from a real regression.
    const PID_POLL_BUDGET: Duration = Duration::from_secs(60);

    /// Poll `reg` until `cell_id`'s PID is registered, the `worker` thread
    /// finishes, or `budget` elapses — whichever happens first.
    ///
    /// The `is_finished` check is what keeps a genuine failure fast: once the
    /// cell has ended, no later poll can make a PID appear, so there is no
    /// reason to burn the remaining budget. That in turn is what lets `budget`
    /// be large enough to absorb heavy-load startup without making a real
    /// breakage slow to detect.
    fn await_registered_pid(
        reg: &ProcRegistry,
        squad_id: &str,
        cell_id: &str,
        worker: &std::thread::JoinHandle<RunnerResult>,
        budget: Duration,
    ) -> Option<u32> {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(pid) = reg.pid_of(squad_id, cell_id) {
                return Some(pid);
            }
            if worker.is_finished() || Instant::now() >= deadline {
                return reg.pid_of(squad_id, cell_id);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The real `run_via_tmux_attempt` path unconditionally persists a pane
    /// snapshot to the *real* `state_dir()` (`~/.ralphus/pane_snapshots`),
    /// not a test-isolated location — appropriate for production (the whole
    /// point is durability past the process/daemon lifetime, see
    /// `crate::tmux::write_pane_snapshot`'s doc comment), but a live-tmux
    /// test using this crate's real `SubprocessRunner` unavoidably writes
    /// there too. Removes the one file this test's own deterministic session
    /// name would have produced, on drop, so running the suite doesn't leave
    /// test fixtures behind in the user's real state directory.
    ///
    /// Also kills the real tmux session itself on drop (RAL-177 AC #3) —
    /// covers both a normal test-fn return and a panic/unwind mid-test.
    /// Fields are owned `String`s (not `&'static str`) so each test can pass
    /// its own [`crate::tmux::unique_test_tag`]-derived, per-invocation-
    /// unique run_id instead of a hardcoded literal.
    struct SnapshotCleanup {
        squad_id: String,
        task: String,
        cell_id: String,
    }
    impl Drop for SnapshotCleanup {
        fn drop(&mut self) {
            let name = crate::tmux::session_name(&self.squad_id, &self.task, &self.cell_id);
            let _ = std::fs::remove_file(crate::tmux::pane_snapshot_path(&name));
            if let Ok(tmux) = Tmux::resolve() {
                let _ = tmux.kill_session(&name);
            }
        }
    }

    #[cfg_attr(
        windows,
        ignore = "opt-in: real tmux/psmux session, can flake under concurrent load on Windows, see PSMUX_CRASH_NOTES.local.md"
    )]
    #[test]
    fn live_tmux_run_via_tmux_full_roundtrip() {
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-1");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "fake-session".to_string(),
        };
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), FAKE_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: None,
            detachments: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_proof(
            &run_id,
            "build",
            "fake-session",
            &cwd,
            "do something",
            "claude",
            None,
            Some(60),
            None,
            None,
        );
        let result = runner.run(&spec);
        assert!(result.is_done(), "expected done, got: {result:?}");
        assert_eq!(result.tokens_in, 1);
        assert_eq!(result.tokens_out, 2);
        assert_eq!(result.summary, "fake");
    }

    #[cfg_attr(
        windows,
        ignore = "opt-in: real tmux/psmux session, can flake under concurrent load on Windows, see PSMUX_CRASH_NOTES.local.md"
    )]
    #[test]
    fn live_tmux_command_kind_spec_runs_via_tmux() {
        // RAL-151: a `command`-kind spec (no `prompt`) must run tmux-wrapped
        // exactly like a `prompt`-kind one now — this is the "blanket, no
        // opt-in" behavior the ticket calls for, so a live view is available
        // for command cells/proof steps too. Reuses the same fake
        // runner as `live_tmux_run_via_tmux_full_roundtrip`; the only
        // difference is `command` is set and `prompt` is not.
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-cmd");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "fake-command-session".to_string(),
        };
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), FAKE_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: None,
            detachments: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_command_proof(
            &run_id,
            "build",
            "fake-command-session",
            &cwd,
            "echo hi",
            "claude",
            Some(60),
        );
        assert!(spec.prompt.is_none(), "this must be a command-kind spec");
        let result = runner.run(&spec);
        assert!(result.is_done(), "expected done, got: {result:?}");
        assert_eq!(result.summary, "fake");
    }

    #[cfg_attr(
        windows,
        ignore = "opt-in: real tmux/psmux session, can flake under concurrent load on Windows, see PSMUX_CRASH_NOTES.local.md"
    )]
    #[test]
    fn live_tmux_registers_and_clears_pid_for_a_command_kind_spec() {
        // RAL-151 follow-up: now that `command`-kind specs run tmux-wrapped
        // too (no raw child process left to register a PID from directly),
        // PID registration for the resource view (RAL-11) comes from
        // `crate::tmux::find_server_pid` instead. That lookup is Windows-only
        // (see its doc comment), so only assert the "registered while
        // running" half there; "cleared after" holds on every platform.
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-pid");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "pid-session".to_string(),
        };
        let reg = crate::procreg::ProcRegistry::new();
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), HANGING_RUNNER_SCRIPT.to_string()],
            registry: Some(reg.clone()),
            cartographer: None,
            detachments: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        // Backstop only — see the identical note on
        // `live_tmux_registers_pid_while_the_subprocess_is_alive_and_clears_it_after`.
        // Must stay above `PID_POLL_BUDGET` so a slow-starting session never
        // races its own timeout-kill against the poll loop (the RAL-171
        // failure signature); the test itself cancels on observation, so this
        // value costs no runtime.
        let spec = RunnerSpec::for_command_proof(
            &run_id,
            "build",
            "pid-session",
            &cwd,
            "noop",
            "claude",
            Some(120),
        );
        let run_id_for_poll = run_id.clone();
        // Cancel once the PID has been observed rather than waiting out
        // `timeout_sec`: this test's runtime then tracks how long startup
        // actually took, not the timeout budget, so that budget can be set
        // generously without making the test slow.
        let cancel = CancelToken::new();
        let cancel_for_worker = cancel.clone();
        let worker = std::thread::spawn(move || runner.run_cancellable(&spec, &cancel_for_worker));

        if cfg!(target_os = "windows") {
            let seen = await_registered_pid(
                &reg,
                &run_id_for_poll,
                "pid-session",
                &worker,
                PID_POLL_BUDGET,
            );
            assert!(
                seen.is_some(),
                "PID should be registered while the tmux-wrapped session runs"
            );
        } else {
            // No process-table lookup off Windows, so there is nothing to wait
            // for -- just let the session get far enough to have something to
            // tear down before cancelling.
            std::thread::sleep(Duration::from_millis(300));
        }
        cancel.cancel();

        let _ = worker.join();
        assert_eq!(
            reg.pid_of(&run_id, "pid-session"),
            None,
            "PID should be cleared once the tmux-wrapped session ends"
        );
    }

    #[cfg_attr(
        windows,
        ignore = "opt-in: real tmux/psmux session, can flake under concurrent load on Windows, see PSMUX_CRASH_NOTES.local.md"
    )]
    #[test]
    fn live_tmux_missing_program_times_out_instead_of_spawn_error() {
        // Once a `command`-kind spec is spawned via `send-keys`/`respawn-pane`
        // inside a tmux pane rather than as a direct child process, an
        // unresolvable program no longer surfaces as an immediate "could not
        // spawn" error (there's nothing left in this crate that calls
        // `Command::spawn` for it) — the pane just shows a shell error and
        // the daemon's poll loop bounds the wait with the spec's own
        // timeout, exactly as it would for any other stuck session.
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-missing");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "missing-session".to_string(),
        };
        let runner = SubprocessRunner::new("definitely-not-a-real-program-xyz");
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_command_proof(
            &run_id,
            "build",
            "missing-session",
            &cwd,
            "noop",
            "claude",
            Some(1),
        );
        let result = runner.run(&spec);
        assert!(!result.is_done());
        assert!(result.error.unwrap().contains("timed out"));
    }

    #[cfg_attr(
        windows,
        ignore = "opt-in: real tmux/psmux session, can flake under concurrent load on Windows, see PSMUX_CRASH_NOTES.local.md"
    )]
    #[test]
    fn live_tmux_run_via_tmux_times_out() {
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-2");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "hanging-session".to_string(),
        };
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), HANGING_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: None,
            detachments: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_proof(
            &run_id,
            "build",
            "hanging-session",
            &cwd,
            "do something",
            "claude",
            None,
            Some(1),
            None,
            None,
        );
        let started = Instant::now();
        let result = runner.run(&spec);
        assert!(!result.is_done());
        assert!(result.error.unwrap().contains("timed out"));
        // Generous ceiling: this only guards against a genuine hang, not
        // brisk enforcement -- the actual 1s budget is checked by the log
        // line above firing near-instantly. The subsequent tmux/subprocess
        // kill+reap this measures also includes can itself take tens of
        // seconds when this test runs alongside the rest of the suite's
        // git/tmux subprocess load, so a tight bound here is a false alarm
        // waiting to happen, not a real regression signal.
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "timeout enforcement should eventually fire, took {:?}",
            started.elapsed()
        );
    }

    #[cfg_attr(
        windows,
        ignore = "opt-in: real tmux/psmux session, can flake under concurrent load on Windows, see PSMUX_CRASH_NOTES.local.md"
    )]
    #[test]
    fn live_tmux_run_via_tmux_is_cancellable() {
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-3");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "cancel-session".to_string(),
        };
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), HANGING_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: None,
            detachments: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_proof(
            &run_id,
            "build",
            "cancel-session",
            &cwd,
            "do something",
            "claude",
            None,
            None,
            None,
            None,
        );
        let cancel = CancelToken::new();
        let cancel_clone = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel_clone.cancel();
        });
        let result = runner.run_cancellable(&spec, &cancel);
        assert!(!result.is_done());
        assert_eq!(result.error.as_deref(), Some("cancelled"));
    }

    #[cfg_attr(
        windows,
        ignore = "opt-in: real tmux/psmux session, can flake under concurrent load on Windows, see PSMUX_CRASH_NOTES.local.md"
    )]
    #[test]
    fn live_tmux_cancel_kills_the_real_process_tree() {
        // RAL-321: `Tmux::kill_session` used to only free the tmux/psmux
        // session *name* -- the pane's real process tree (the psmux server
        // and everything it spawned into the pane) kept running underneath
        // it. `live_tmux_run_via_tmux_is_cancellable` above only asserts the
        // runner reports "cancelled"; this asserts the actual OS process is
        // gone too, via `crate::tmux::find_server_pid` +
        // `crate::tmux::test_pid_is_alive` (both Windows-only, see their own
        // doc comments -- there is no equivalent confinement on Unix, see
        // `tmux.rs`'s module doc comment).
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-killtree");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "killtree-session".to_string(),
        };
        let session_name = crate::tmux::session_name(&run_id, "build", "killtree-session");
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), HANGING_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: None,
            detachments: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_proof(
            &run_id,
            "build",
            "killtree-session",
            &cwd,
            "do something",
            "claude",
            None,
            None,
            None,
            None,
        );
        let cancel = CancelToken::new();
        let cancel_for_worker = cancel.clone();
        let worker = std::thread::spawn(move || runner.run_cancellable(&spec, &cancel_for_worker));

        if !cfg!(target_os = "windows") {
            // No process-table lookup off Windows (`find_server_pid` always
            // returns `None` there), so there is nothing to assert real OS
            // death against -- still exercise the cancel path itself.
            std::thread::sleep(Duration::from_millis(300));
            cancel.cancel();
            let result = worker.join().unwrap();
            assert!(!result.is_done());
            assert_eq!(result.error.as_deref(), Some("cancelled"));
            return;
        }

        let deadline = Instant::now() + PID_POLL_BUDGET;
        let pid = loop {
            if let Some(pid) = crate::tmux::find_server_pid(&session_name) {
                break Some(pid);
            }
            if worker.is_finished() || Instant::now() >= deadline {
                break crate::tmux::find_server_pid(&session_name);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        .expect("tmux server pid should be found while the session is running");
        assert!(
            crate::tmux::test_pid_is_alive(pid),
            "tmux server pid={pid} should be alive before cancel"
        );

        cancel.cancel();
        let result = worker.join().unwrap();
        assert!(!result.is_done());
        assert_eq!(result.error.as_deref(), Some("cancelled"));

        // Bounded poll, not a fixed sleep: `kill_session`'s own wait loop
        // already bounds how long `has-session` takes to clear, so this
        // should resolve almost immediately once `run_cancellable` returns --
        // the point of RAL-321 is that this used to never become true at all.
        let death_deadline = Instant::now() + Duration::from_secs(10);
        while crate::tmux::test_pid_is_alive(pid) && Instant::now() < death_deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            !crate::tmux::test_pid_is_alive(pid),
            "tmux server pid={pid} should be dead after cancel (RAL-321: process-tree confinement)"
        );
    }

    #[cfg_attr(windows, ignore = "CI-only on Windows: exercises a real psmux server")]
    #[test]
    fn live_tmux_run_via_tmux_is_detachable() {
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-detach");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "detach-session".to_string(),
        };
        let detachments = crate::cancel::Detachments::new();
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), HANGING_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: None,
            detachments: Some(detachments.clone()),
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec::for_proof(
            &run_id,
            "build",
            "detach-session",
            &cwd,
            "do something",
            "claude",
            None,
            None,
            None,
            None,
        );
        // RAL-288 Stage 6: an HTTP handler doesn't have the runner's own
        // computed session name -- it only knows (squad_id, task, cell_id),
        // same as every other tmux-keyed endpoint (`attach_tmux_terminal`,
        // `capture_pane_reply`) already recomputes it.
        let session_name = crate::tmux::session_name(&spec.squad_id, &spec.task, &spec.cell_id);
        let _cleanup_session = crate::tmux::KillSessionOnDrop(session_name.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            detachments.cancel(&session_name);
        });
        let result = runner.run_cancellable(&spec, &CancelToken::never());
        assert!(!result.is_done());
        assert!(result.is_detached());
        assert_eq!(result.error, None);
    }

    /// A fake runner for the auto-reattach test (RAL-102 follow-up): reads
    /// its own spec file back and behaves differently depending on whether
    /// `resume_agent_session_id` is set. On a fresh (non-resumed) attempt it
    /// announces a agent_session_id over the `RALPHUS_EVENT:` marker — same
    /// as the real `claude-code` backend's stream-json init event — then
    /// hangs, standing in for a cell whose tmux pane the test kills out
    /// from under it. On a resumed attempt (the daemon's own retry, which
    /// sets `resume_agent_session_id` in the spec it writes) it completes
    /// immediately with a distinguishable summary, proving the reattach
    /// actually happened and carried the right id.
    const REATTACH_RUNNER_SCRIPT: &str = "import sys,json,time; \
        sp=sys.argv[sys.argv.index(\"--result-file\")-1]; \
        rp=sys.argv[sys.argv.index(\"--result-file\")+1]; \
        spec=json.load(open(sp)); \
        resumed=spec.get(\"resume_agent_session_id\"); \
        sys.stderr.write(\"RALPHUS_EVENT: \" + json.dumps({\"source\":\"llm-invoke\",\"message\":\"sid known\",\"payload\":{\"agent_session_id\":\"reattach-test-sid\"}}) + \"\\n\"); \
        sys.stderr.flush(); \
        (open(rp,\"w\").write(json.dumps({\"status\":\"done\",\"tokens_in\":1,\"tokens_out\":1,\"cost_usd\":0.0,\"summary\":\"resumed-and-done resume_from=\"+str(resumed),\"error\":None,\"proofed\":None,\"agent_session_id\":\"reattach-test-sid\"})), \
         print(\"RALPHUS_TMUX\" + \"_DONE: done\")) if resumed else time.sleep(30)";

    // Ignored on Windows: races the daemon's poll loop against a real
    // psmux session kill, and has been observed to flake deterministically
    // against the still-unexplained pane/scrollback loss documented in
    // PSMUX_CRASH_NOTES.local.md (RAL-159 proof run, run-000000000147) even
    // with LIVE_TMUX_TEST_LOCK held. Not gated on CI (ubuntu-latest has no
    // tmux, so `tmux_and_python_available()` already skips it there).
    #[cfg_attr(
        windows,
        ignore = "flaky: races psmux session kill against the daemon poll loop, see PSMUX_CRASH_NOTES.local.md"
    )]
    #[test]
    fn live_tmux_run_via_tmux_reattaches_after_session_vanishes() {
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        // This test explicitly kills a real tmux session mid-flight and races
        // the daemon's own poll loop against real wall-clock timing to prove
        // the reattach — serialize against every other live-tmux test in the
        // suite so a concurrently running one's own session creation/teardown
        // (or plain CPU contention from it) can't be what makes the daemon's
        // poll thread miss the window. Observed flaking under a full
        // `cargo test --lib` run even with a generous post-detection buffer;
        // isolating this test from sibling live-tmux tests is the fix that
        // actually addresses the contention rather than just widening timeouts.
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-reattach");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "reattach-session".to_string(),
        };
        let store = Store::open_in_memory().expect("open in-memory store");
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), REATTACH_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: Some(Arc::new(Mutex::new(store))),
            detachments: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "reattach-session".to_string(),
            cwd,
            prompt: Some("do something".to_string()),
            command: None,
            agent: "claude-code".to_string(),
            executable: None,
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            timeout_sec: Some(30),
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            proof: false,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            env_overrides: BTreeMap::new(),
            machine: None,
            tool_arg_truncate_chars: None,
            thrash_max_compactions: None,
            thrash_min_turn_gap: None,
            allow_personal_settings: false,
            allow_personal_memory: false,
        };
        let session_name = crate::tmux::session_name(&spec.squad_id, &spec.task, &spec.cell_id);

        let runner = Arc::new(runner);
        let runner_clone = Arc::clone(&runner);
        let spec_clone = spec.clone();
        let started = Instant::now();
        let handle = std::thread::spawn(move || runner_clone.run(&spec_clone));

        // Wait for the first attempt's session to appear and its pane to
        // actually show the llm-invoke event line (polled directly, rather
        // than a fixed sleep, so this isn't flaky under parallel-test-run CPU
        // contention — Python startup alone can take much longer than a
        // guessed delay when other live-tmux tests are competing for the
        // same machine), then kill it out from under the daemon — simulating
        // the still-unexplained "mystery" pane loss from
        // PSMUX_CRASH_NOTES.local.md.
        let tmux = Tmux::resolve().expect("tmux resolves (already checked available)");
        let appeared = Instant::now();
        loop {
            assert!(
                appeared.elapsed() < Duration::from_secs(30),
                "first attempt's tmux session never showed the llm-invoke event line"
            );
            if let Ok(pane) = tmux.capture_pane(&session_name, 2000) {
                if pane.contains("RALPHUS_EVENT:") {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // The line being visible to *this* capture-pane call doesn't mean the
        // daemon's own independent poll loop (TMUX_POLL_INTERVAL, 500ms) has
        // run its own capture-pane and forwarded it yet — give it a couple of
        // full poll cycles' worth of buffer before killing, so the daemon
        // reliably gets the event before the pane disappears out from under
        // it. `LIVE_TMUX_TEST_LOCK` (acquired above) removes contention from
        // sibling live-tmux tests specifically, but not from the ~480 other,
        // unrelated tests `cargo test --lib` also runs concurrently in other
        // threads — measured 1-in-5 full-suite flakes at 3x even with the
        // lock held; 5x has been reliable across repeated full-suite runs.
        std::thread::sleep(TMUX_POLL_INTERVAL * 5);
        tmux.kill_session(&session_name)
            .expect("kill the first attempt's session");

        let result = handle.join().expect("runner thread panicked");
        assert!(
            result.is_done(),
            "expected the reattach to succeed, got: {result:?}"
        );
        assert_eq!(
            result.summary, "resumed-and-done resume_from=reattach-test-sid",
            "the second attempt must have been invoked with the agent_session_id captured from the first"
        );
        assert!(
            // Loose sanity bound, not a precise timing assertion -- mainly to
            // catch a hang. `spec.timeout_sec` is 30s and this test's own
            // event-line wait is bounded the same, so a slow-but-not-hung run
            // under CPU contention (see the wait loop above) can legitimately
            // take a while.
            started.elapsed() < Duration::from_secs(60),
            "reattach should complete, took {:?}",
            started.elapsed()
        );
    }

    /// Same reattach flow as
    /// [`live_tmux_run_via_tmux_reattaches_after_session_vanishes`], but for
    /// `agent = "codex"` -- proves the gate widened alongside `CodexBackend`
    /// gaining a real resume mechanism (`codex exec resume <thread_id>`)
    /// actually activates the retry path for Codex, not just Claude Code.
    /// `REATTACH_RUNNER_SCRIPT` is itself agent-agnostic (it only reads
    /// `resume_agent_session_id` back from the spec file), so the only
    /// thing under test here is the `attempt_spec.agent` gate in
    /// `run_via_tmux`.
    // Ignored on Windows: same psmux session-kill/poll-loop race as
    // live_tmux_run_via_tmux_reattaches_after_session_vanishes above, just
    // parameterized for agent = "codex" -- see that test's comment and
    // PSMUX_CRASH_NOTES.local.md for details.
    #[cfg_attr(
        windows,
        ignore = "flaky: races psmux session kill against the daemon poll loop, see PSMUX_CRASH_NOTES.local.md"
    )]
    #[test]
    fn live_tmux_run_via_tmux_reattaches_after_session_vanishes_for_codex() {
        if !tmux_and_python_available() {
            println!("SKIP: tmux and/or python not found on PATH");
            return;
        }
        let _tmux_guard = crate::tmux::LIVE_TMUX_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::tmux::sweep_dead_test_sessions_once();
        let run_id = crate::tmux::unique_test_tag("run-tmux-reattach-codex");
        let _cleanup = SnapshotCleanup {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "reattach-session-codex".to_string(),
        };
        let store = Store::open_in_memory().expect("open in-memory store");
        let runner = SubprocessRunner {
            program: "python".to_string(),
            args: vec!["-c".to_string(), REATTACH_RUNNER_SCRIPT.to_string()],
            registry: None,
            cartographer: Some(Arc::new(Mutex::new(store))),
            detachments: None,
        };
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let spec = RunnerSpec {
            squad_id: run_id.clone(),
            task: "build".to_string(),
            cell_id: "reattach-session-codex".to_string(),
            cwd,
            prompt: Some("do something".to_string()),
            command: None,
            agent: "codex".to_string(),
            executable: None,
            model: None,
            system_prompt: None,
            system_prompt_position: None,
            timeout_sec: Some(30),
            budget_tokens: None,
            maximum_budget_usd: None,
            maximum_context: None,
            auto_compact_threshold: None,
            maximum_tool_output_tokens: None,
            proof: false,
            trace_context: None,
            resume_agent_session_id: None,
            assigned_agent_session_id: None,
            env_overrides: BTreeMap::new(),
            machine: None,
            tool_arg_truncate_chars: None,
            thrash_max_compactions: None,
            thrash_min_turn_gap: None,
            allow_personal_settings: false,
            allow_personal_memory: false,
        };
        let session_name = crate::tmux::session_name(&spec.squad_id, &spec.task, &spec.cell_id);

        let runner = Arc::new(runner);
        let runner_clone = Arc::clone(&runner);
        let spec_clone = spec.clone();
        let started = Instant::now();
        let handle = std::thread::spawn(move || runner_clone.run(&spec_clone));

        let tmux = Tmux::resolve().expect("tmux resolves (already checked available)");
        let appeared = Instant::now();
        loop {
            assert!(
                appeared.elapsed() < Duration::from_secs(30),
                "first attempt's tmux session never showed the llm-invoke event line"
            );
            if let Ok(pane) = tmux.capture_pane(&session_name, 2000) {
                if pane.contains("RALPHUS_EVENT:") {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(TMUX_POLL_INTERVAL * 5);
        tmux.kill_session(&session_name)
            .expect("kill the first attempt's session");

        let result = handle.join().expect("runner thread panicked");
        assert!(
            result.is_done(),
            "expected the reattach to succeed, got: {result:?}"
        );
        assert_eq!(
            result.summary, "resumed-and-done resume_from=reattach-test-sid",
            "the second attempt must have been invoked with the agent_session_id captured from the first"
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "reattach should complete, took {:?}",
            started.elapsed()
        );
    }

    /// RAL-377: a missing/misconfigured `RALPHUS_RUNNER_CMD` must fail loudly
    /// up front rather than only surfacing once `run_via_tmux` tries and
    /// fails to spawn it -- see `SubprocessRunner::preflight_runner_executable`.
    #[test]
    fn preflight_runner_executable_fails_for_a_missing_program() {
        let runner = SubprocessRunner::new("ralphus-runner-does-not-exist-ral-377");
        let err = runner
            .preflight_runner_executable(None)
            .expect_err("a nonexistent program must fail preflight");
        assert!(
            err.contains("ralphus-runner-does-not-exist-ral-377"),
            "error should name the executable it looked for: {err}"
        );
    }

    #[test]
    fn preflight_runner_executable_succeeds_for_a_real_absolute_path() {
        let exe = std::env::current_exe().expect("current test binary has a path");
        let runner = SubprocessRunner {
            program: exe.to_string_lossy().into_owned(),
            args: Vec::new(),
            registry: None,
            cartographer: None,
            detachments: None,
        };
        runner
            .preflight_runner_executable(None)
            .expect("the running test binary is itself a real, executable file");
    }
}
