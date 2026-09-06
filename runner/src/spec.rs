//! The daemon<->runner JSON wire contract.
//! [`CellSpec`] is read from stdin,
//! [`CellResult`] is written to stdout. Matches `daemon/src/runner.rs`'s
//! `RunnerSpec`/`RunnerResult` field-for-field (that file documents itself as
//! the daemon-side mirror of this wire format).

use serde_json::Value;

/// A spec field was missing, the wrong JSON type, or the `prompt`/`command`
/// XOR invariant was violated. Mirrors Python's `SpecError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecError(pub String);

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SpecError {}

/// The cell the daemon wants run, as received on stdin.
#[derive(Debug, Clone, PartialEq)]
pub struct CellSpec {
    pub squad_id: String,
    pub task: String,
    pub cell_id: String,
    pub cwd: String,
    /// Exactly one of `prompt`/`command` is `Some` -- enforced in [`Self::from_json`].
    pub prompt: Option<String>,
    pub command: Option<String>,
    pub agent: String,
    pub executable: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub system_prompt_position: Option<String>,
    pub args: Vec<String>,
    pub budget_tokens: Option<u64>,
    /// RAL-304: resolved context-window token limit. `None` for no cap, or
    /// when unsupported by `agent` (the daemon rejects this combination at
    /// submit time, per `core::validate`'s `agent_supports_maximum_context`).
    pub maximum_context: Option<u64>,
    /// RAL-304: resolved auto-compact trigger threshold in tokens. `None`
    /// for no explicit threshold, or when unsupported by `agent` (the
    /// daemon rejects this combination at submit time, per
    /// `core::validate`'s `agent_supports_auto_compact_threshold`). Accepted
    /// by a wider set of backends than `maximum_context` -- e.g. claude-code
    /// supports this field but not that one.
    pub auto_compact_threshold: Option<u64>,
    pub timeout_sec: Option<u64>,
    pub proof: bool,
    pub trace_context: Option<String>,
    pub resume_agent_session_id: Option<String>,
    /// A session id the daemon pre-generated and persisted before this cell
    /// started (RAL-288 Stage 1). The claude-code backend passes it as
    /// `--session-id` when not resuming, so "Open Agent" has something to
    /// attach to from the very first moment of a run instead of waiting on
    /// the backend's own init event. Other backends ignore it (mirrors
    /// `resume_agent_session_id`'s "hand-rolled backends accept and ignore
    /// it" precedent).
    pub assigned_agent_session_id: Option<String>,
    /// RAL-303: the daemon's resolved `[live_view] tool_arg_truncate_chars`,
    /// forwarded so the claude-code backend's `format_tool_input` knows how
    /// much of a `tool_use` argument to render into the Live View tmux pane
    /// before truncating. `None` (e.g. a hand-authored spec that omits the
    /// key) falls back to the backend's own default.
    pub tool_arg_truncate_chars: Option<u32>,
    /// RAL-339: resolved `.ralphus.toml` `[thrash]` thresholds -- N (compact
    /// count) and M (turn-gap) -- forwarded from the daemon the same way
    /// `tool_arg_truncate_chars` is. `None` falls back to
    /// `crate::thrash`'s own defaults.
    pub thrash_max_compactions: Option<u32>,
    pub thrash_min_turn_gap: Option<u32>,
    /// RAL-333: resolved cap on how many tokens a single tool-call output may
    /// inject into the agent's context. `None` for no cap, or when
    /// unsupported by `agent` (the daemon rejects this combination at submit
    /// time, per `core::validate`'s `agent_supports_maximum_tool_output_tokens`).
    pub maximum_tool_output_tokens: Option<u64>,
    /// RAL-336: whether this cell's agent session may load the operator's
    /// personal settings/config (Claude Code's `~/.claude` settings, Codex's
    /// `~/.codex/config.toml`, Pi's on-disk config). Defaults to `false`
    /// (isolated) when omitted, per the daemon's `resolve_agent_isolation`.
    pub allow_personal_settings: bool,
    /// RAL-336: whether this cell's agent session may load the operator's
    /// personal cross-project memory (e.g. Claude Code's global `CLAUDE.md`).
    /// Defaults to `false` (isolated) when omitted.
    pub allow_personal_memory: bool,
}

impl CellSpec {
    /// Parse and validate a `CellSpec` from raw JSON text (the daemon's
    /// stdin payload). Rejects a wrong JSON type per field -- including a
    /// bare `bool` where an `int` is expected -- exactly as the
    /// `_require_str`/`_opt_int`/etc. helpers do, and enforces that exactly
    /// one of `prompt`/`command` is set.
    pub fn from_json(text: &str) -> Result<Self, SpecError> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| SpecError(format!("invalid JSON: {e}")))?;
        let obj = value
            .as_object()
            .ok_or_else(|| SpecError("spec must be a JSON object".to_string()))?;

        let squad_id = require_str(obj, "squad_id")?;
        let task = require_str(obj, "task")?;
        let cell_id = require_str(obj, "cell_id")?;
        let cwd = require_str(obj, "cwd")?;
        let prompt = opt_str(obj, "prompt")?;
        let command = opt_str(obj, "command")?;
        let agent = opt_str(obj, "agent")?.unwrap_or_else(|| "claude".to_string());
        let executable = opt_str(obj, "executable")?;
        let model = opt_str(obj, "model")?;
        let system_prompt = opt_str(obj, "system_prompt")?;
        let system_prompt_position = opt_str(obj, "system_prompt_position")?;
        let args = str_list(obj, "args")?;
        let budget_tokens = opt_uint(obj, "budget_tokens")?;
        let maximum_context = opt_uint(obj, "maximum_context")?;
        let auto_compact_threshold = opt_uint(obj, "auto_compact_threshold")?;
        let timeout_sec = opt_uint(obj, "timeout_sec")?;
        let proof = opt_bool(obj, "proof")?.unwrap_or(false);
        let trace_context = opt_str(obj, "trace_context")?;
        let resume_agent_session_id = opt_str(obj, "resume_agent_session_id")?;
        let assigned_agent_session_id = opt_str(obj, "assigned_agent_session_id")?;
        let tool_arg_truncate_chars = opt_u32(obj, "tool_arg_truncate_chars")?;
        let thrash_max_compactions = opt_u32(obj, "thrash_max_compactions")?;
        let thrash_min_turn_gap = opt_u32(obj, "thrash_min_turn_gap")?;
        let maximum_tool_output_tokens = opt_uint(obj, "maximum_tool_output_tokens")?;
        let allow_personal_settings = opt_bool(obj, "allow_personal_settings")?.unwrap_or(false);
        let allow_personal_memory = opt_bool(obj, "allow_personal_memory")?.unwrap_or(false);

        if prompt.is_some() == command.is_some() {
            return Err(SpecError(
                "exactly one of prompt/command must be set".to_string(),
            ));
        }

        Ok(Self {
            squad_id,
            task,
            cell_id,
            cwd,
            prompt,
            command,
            agent,
            executable,
            model,
            system_prompt,
            system_prompt_position,
            args,
            budget_tokens,
            maximum_context,
            auto_compact_threshold,
            timeout_sec,
            proof,
            trace_context,
            resume_agent_session_id,
            assigned_agent_session_id,
            tool_arg_truncate_chars,
            thrash_max_compactions,
            thrash_min_turn_gap,
            maximum_tool_output_tokens,
            allow_personal_settings,
            allow_personal_memory,
        })
    }
}

fn field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

fn require_str(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, SpecError> {
    match field(obj, key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(SpecError(format!("{key} must be a string"))),
        None => Err(SpecError(format!("missing required field: {key}"))),
    }
}

fn opt_str(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>, SpecError> {
    match field(obj, key) {
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(SpecError(format!("{key} must be a string"))),
        None => Ok(None),
    }
}

fn opt_bool(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<bool>, SpecError> {
    match field(obj, key) {
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(SpecError(format!("{key} must be a bool"))),
        None => Ok(None),
    }
}

/// `Value::Bool` is deliberately rejected even though `serde_json` would
/// coerce it via `as_u64` in some crate versions -- Python's `bool` is a
/// subtype of `int`, so `_opt_int` explicitly excludes it, and this mirrors
/// that exclusion.
fn opt_uint(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u64>, SpecError> {
    match field(obj, key) {
        Some(Value::Bool(_)) => Err(SpecError(format!("{key} must be an integer"))),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(Some(n)),
            None => Err(SpecError(format!("{key} must be an integer"))),
        },
        None => Ok(None),
    }
}

/// Like [`opt_uint`], narrowed to `u32` -- used for `tool_arg_truncate_chars`
/// (RAL-303), which is always a small character count, never a value that
/// needs the full `u64` range `budget_tokens`/`timeout_sec` allow for.
fn opt_u32(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u32>, SpecError> {
    match opt_uint(obj, key)? {
        Some(n) => u32::try_from(n)
            .map(Some)
            .map_err(|_| SpecError(format!("{key} must fit in a 32-bit integer"))),
        None => Ok(None),
    }
}

fn str_list(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Vec<String>, SpecError> {
    match field(obj, key) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| SpecError(format!("{key} entries must be strings")))
            })
            .collect(),
        Some(_) => Err(SpecError(format!("{key} must be a list"))),
        None => Ok(Vec::new()),
    }
}

/// The outcome the daemon reads back from stdout. Mirrors the daemon's
/// `RunnerResult`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CellResult {
    pub status: String,
    #[serde(default)]
    pub tokens_in: i64,
    #[serde(default)]
    pub tokens_out: i64,
    /// RAL-326: prompt-cache write/read tokens, kept out of `tokens_in` so
    /// that field keeps meaning exactly what it always has. Zero for a
    /// backend whose harness reports no cache breakdown.
    #[serde(default)]
    pub cache_creation_tokens: i64,
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cost_usd: f64,
    /// RAL-326: set when `cost_usd`/the token counts are a *live snapshot*
    /// rather than the backend's own authoritative final accounting -- the
    /// cell's process was lost or killed before a terminal usage event
    /// arrived, so the daemon fell back to the last mid-run estimate. The
    /// board renders this as an "≈ estimated" badge so a snapshot value is
    /// never read as a settled bill.
    #[serde(default)]
    pub cost_is_estimated: bool,
    #[serde(default)]
    pub summary: String,
    pub error: Option<String>,
    pub proofed: Option<bool>,
    pub agent_session_id: Option<String>,
    pub ghost: Option<String>,
}

impl CellResult {
    #[must_use]
    pub fn done(summary: impl Into<String>) -> Self {
        Self {
            status: "done".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: summary.into(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }

    #[must_use]
    pub fn failed(error: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            status: "failed".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            cost_is_estimated: false,
            summary: summary.into(),
            error: Some(error.into()),
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }

    /// RAL-288 Stage 6: a deliberate human-triggered detach mid-task, so a
    /// real interactive session can take over. Neither success nor failure
    /// -- the cell's actual goal may be nowhere near finished. Carries
    /// whatever usage/session-id was captured live up to the detach point,
    /// the same way [`Self::done`]'s caller fills those in, so the board's
    /// numbers don't regress to zero.
    #[must_use]
    pub fn detached(
        tokens_in: i64,
        tokens_out: i64,
        cache_creation_tokens: i64,
        cache_read_tokens: i64,
        cost_usd: f64,
        agent_session_id: Option<String>,
    ) -> Self {
        Self {
            status: "detached".to_string(),
            tokens_in,
            tokens_out,
            cache_creation_tokens,
            cache_read_tokens,
            cost_usd,
            // A detach carries whatever the live snapshot held at the detach
            // point, never a terminal usage event -- so it is an estimate by
            // construction (RAL-326).
            cost_is_estimated: true,
            summary: String::new(),
            error: None,
            proofed: None,
            agent_session_id,
            ghost: None,
        }
    }

    #[must_use]
    pub fn ok(&self) -> bool {
        self.status == "done"
    }

    /// RAL-288 Stage 6.
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.status == "detached"
    }

    /// Serializes all fields unconditionally (including `None`s) -- the
    /// daemon's `RunnerResult` deserialization uses
    /// `#[serde(default)]` per field, so omitted keys would also work, but
    /// this keeps the two sides wire-identical.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"status":"failed","error":"result serialization failed"}"#.to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> serde_json::Value {
        serde_json::json!({
            "squad_id": "r1",
            "task": "t1",
            "cell_id": "s1",
            "cwd": "/tmp/work",
            "prompt": "do the thing",
        })
    }

    #[test]
    fn parses_minimal_prompt_spec() {
        let spec = CellSpec::from_json(&base().to_string()).unwrap();
        assert_eq!(spec.squad_id, "r1");
        assert_eq!(spec.prompt.as_deref(), Some("do the thing"));
        assert_eq!(spec.command, None);
        assert_eq!(spec.agent, "claude");
        assert!(!spec.proof);
        assert!(spec.args.is_empty());
    }

    #[test]
    fn rejects_neither_prompt_nor_command() {
        let mut v = base();
        v.as_object_mut().unwrap().remove("prompt");
        let err = CellSpec::from_json(&v.to_string()).unwrap_err();
        assert!(err.0.contains("exactly one"));
    }

    #[test]
    fn rejects_both_prompt_and_command() {
        let mut v = base();
        v["command"] = serde_json::json!("echo hi");
        let err = CellSpec::from_json(&v.to_string()).unwrap_err();
        assert!(err.0.contains("exactly one"));
    }

    #[test]
    fn rejects_wrong_type_for_string_field() {
        let mut v = base();
        v["squad_id"] = serde_json::json!(42);
        let err = CellSpec::from_json(&v.to_string()).unwrap_err();
        assert!(err.0.contains("squad_id"));
    }

    #[test]
    fn rejects_bool_for_int_field() {
        let mut v = base();
        v["timeout_sec"] = serde_json::json!(true);
        let err = CellSpec::from_json(&v.to_string()).unwrap_err();
        assert!(err.0.contains("timeout_sec"));
    }

    #[test]
    fn rejects_missing_required_field() {
        let mut v = base();
        v.as_object_mut().unwrap().remove("cwd");
        let err = CellSpec::from_json(&v.to_string()).unwrap_err();
        assert!(err.0.contains("cwd"));
    }

    #[test]
    fn parses_full_spec() {
        let mut v = base();
        v.as_object_mut().unwrap().remove("prompt");
        v["command"] = serde_json::json!("echo hi");
        v["agent"] = serde_json::json!("ollama");
        v["model"] = serde_json::json!("qwen3:8b");
        v["args"] = serde_json::json!(["--flag", "value"]);
        v["budget_tokens"] = serde_json::json!(1000);
        v["timeout_sec"] = serde_json::json!(60);
        v["proof"] = serde_json::json!(true);
        v["tool_arg_truncate_chars"] = serde_json::json!(400);
        v["maximum_tool_output_tokens"] = serde_json::json!(20000);
        let spec = CellSpec::from_json(&v.to_string()).unwrap();
        assert_eq!(spec.command.as_deref(), Some("echo hi"));
        assert_eq!(spec.agent, "ollama");
        assert!(spec.executable.is_none());
        assert_eq!(spec.args, vec!["--flag".to_string(), "value".to_string()]);
        assert_eq!(spec.budget_tokens, Some(1000));
        assert!(spec.proof);
        assert_eq!(spec.tool_arg_truncate_chars, Some(400));
        assert_eq!(spec.maximum_tool_output_tokens, Some(20000));
    }

    #[test]
    fn maximum_tool_output_tokens_is_none_when_omitted() {
        let spec = CellSpec::from_json(&base().to_string()).unwrap();
        assert_eq!(spec.maximum_tool_output_tokens, None);
    }

    #[test]
    fn tool_arg_truncate_chars_is_none_when_omitted() {
        let spec = CellSpec::from_json(&base().to_string()).unwrap();
        assert_eq!(spec.tool_arg_truncate_chars, None);
    }

    #[test]
    fn tool_arg_truncate_chars_rejects_a_value_that_does_not_fit_a_u32() {
        let mut v = base();
        v["tool_arg_truncate_chars"] = serde_json::json!(u64::from(u32::MAX) + 1);
        let err = CellSpec::from_json(&v.to_string()).unwrap_err();
        assert!(err.0.contains("tool_arg_truncate_chars"));
    }

    #[test]
    fn allow_personal_settings_and_memory_default_to_false_when_omitted() {
        let spec = CellSpec::from_json(&base().to_string()).unwrap();
        assert!(!spec.allow_personal_settings);
        assert!(!spec.allow_personal_memory);
    }

    #[test]
    fn allow_personal_settings_and_memory_parse_explicit_true() {
        let mut v = base();
        v["allow_personal_settings"] = serde_json::json!(true);
        v["allow_personal_memory"] = serde_json::json!(true);
        let spec = CellSpec::from_json(&v.to_string()).unwrap();
        assert!(spec.allow_personal_settings);
        assert!(spec.allow_personal_memory);
    }

    #[test]
    fn allow_personal_settings_rejects_wrong_type() {
        let mut v = base();
        v["allow_personal_settings"] = serde_json::json!("yes");
        let err = CellSpec::from_json(&v.to_string()).unwrap_err();
        assert!(err.0.contains("allow_personal_settings"));
    }

    #[test]
    fn cell_result_done_and_failed() {
        let d = CellResult::done("ok");
        assert!(d.ok());
        assert_eq!(d.summary, "ok");
        let f = CellResult::failed("boom", "");
        assert!(!f.ok());
        assert_eq!(f.error.as_deref(), Some("boom"));
    }

    #[test]
    fn to_json_roundtrips_status() {
        let r = CellResult::done("summary text");
        let text = r.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["status"], "done");
        assert_eq!(parsed["summary"], "summary text");
    }
}
