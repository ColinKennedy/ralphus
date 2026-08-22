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
    pub timeout_sec: Option<u64>,
    pub proof: bool,
    pub trace_context: Option<String>,
    pub resume_agent_session_id: Option<String>,
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
        let timeout_sec = opt_uint(obj, "timeout_sec")?;
        let proof = opt_bool(obj, "proof")?.unwrap_or(false);
        let trace_context = opt_str(obj, "trace_context")?;
        let resume_agent_session_id = opt_str(obj, "resume_agent_session_id")?;

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
            timeout_sec,
            proof,
            trace_context,
            resume_agent_session_id,
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
    #[serde(default)]
    pub cost_usd: f64,
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
            cost_usd: 0.0,
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
            cost_usd: 0.0,
            summary: summary.into(),
            error: Some(error.into()),
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }

    #[must_use]
    pub fn ok(&self) -> bool {
        self.status == "done"
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
        let spec = CellSpec::from_json(&v.to_string()).unwrap();
        assert_eq!(spec.command.as_deref(), Some("echo hi"));
        assert_eq!(spec.agent, "ollama");
        assert!(spec.executable.is_none());
        assert_eq!(spec.args, vec!["--flag".to_string(), "value".to_string()]);
        assert_eq!(spec.budget_tokens, Some(1000));
        assert!(spec.proof);
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
