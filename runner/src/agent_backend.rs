//! The tool-calling `ModelBackend`, ported from
//! `cli/src/ralphus/runner/pydantic_backend.py`. Backs the `claude`/
//! `anthropic`/`ollama` agent names -- the "native" backend, as opposed to
//! the external-CLI backends ([`crate::claude_code_backend`],
//! [`crate::codex_backend`]) or the generic [`crate::harness_backend`].

use serde_json::{Value, json};

use crate::backend::{BackendError, BackendOutcome, ModelBackend, RunOptions};
use crate::llm_client::{self, ToolSpec};
use crate::providers;
use crate::tools::Workspace;

const SYSTEM_PROMPT: &str = "You are a headless coding agent. Accomplish the user's task by calling the provided tools (read_file, write_file, run_bash), operating only within the workspace. Do the minimum necessary, then reply with a one-line summary.";

pub struct AgentBackend {
    /// The agent name as it appears in `CellSpec.agent` (`"claude"`,
    /// `"anthropic"`, or `"ollama"`) -- resolved to a provider per-call via
    /// [`providers::resolve`], same as Python's `_build_model`.
    pub agent: String,
}

fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file".to_string(),
            description: "Read a UTF-8 text file relative to the workspace root.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        },
        ToolSpec {
            name: "write_file".to_string(),
            description: "Write UTF-8 text content to a file relative to the workspace root, creating parent directories as needed.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                },
                "required": ["path", "content"],
            }),
        },
        ToolSpec {
            name: "run_bash".to_string(),
            description: "Run a shell command rooted at the workspace directory and return its output.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
            }),
        },
    ]
}

/// First 8 hex chars of the SHA-256 digest -- matches Python's
/// `hashlib.sha256(prompt.encode()).hexdigest()[:8]`.
fn short_prompt_hash(data: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(data)[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

fn dispatch_tool(workspace: &Workspace, name: &str, input: &Value) -> String {
    let result = match name {
        "read_file" => input["path"]
            .as_str()
            .ok_or_else(|| "read_file: missing path argument".to_string())
            .and_then(|p| workspace.read_file(p).map_err(|e| e.to_string())),
        "write_file" => {
            let path = input["path"].as_str();
            let content = input["content"].as_str();
            match (path, content) {
                (Some(p), Some(c)) => workspace
                    .write_file(p, c)
                    .map(|()| "ok".to_string())
                    .map_err(|e| e.to_string()),
                _ => Err("write_file: missing path/content argument".to_string()),
            }
        }
        "run_bash" => input["command"]
            .as_str()
            .ok_or_else(|| "run_bash: missing command argument".to_string())
            .and_then(|c| {
                workspace
                    .run_bash(c, None)
                    .map(|out| {
                        format!(
                            "exit_code={}\nstdout:\n{}\nstderr:\n{}",
                            out.exit_code, out.stdout, out.stderr
                        )
                    })
                    .map_err(|e| e.to_string())
            }),
        other => Err(format!("unknown tool: {other}")),
    };
    match result {
        Ok(s) => s,
        Err(e) => format!("error: {e}"),
    }
}

impl ModelBackend for AgentBackend {
    fn run(
        &self,
        prompt: &str,
        workspace: &Workspace,
        options: &RunOptions<'_>,
    ) -> Result<BackendOutcome, BackendError> {
        let provider = providers::resolve(&self.agent, options.model)
            .map_err(|e| BackendError(e.to_string()))?;

        let system_prompt = match options.append_system_prompt {
            Some(extra) if !extra.is_empty() => format!("{SYSTEM_PROMPT}\n\n{extra}"),
            _ => SYSTEM_PROMPT.to_string(),
        };
        let tools = tool_specs();
        let timeout = options.timeout_sec.map(std::time::Duration::from_secs);

        // RAL-288 Stage 5: Cartographer-only, not a plain `eprintln!` -- see
        // `execute.rs::log_llm_start`'s doc comment for why. No `CellSpec`
        // is in scope here (`ModelBackend::run` doesn't take one), so this
        // relies on the daemon's own squad/task/cell fallback when
        // forwarding, same as `main.rs::finish`'s result-file-write-failure
        // diagnostic.
        crate::cartographer::emit(
            "llm-invoke",
            "start",
            "info",
            crate::cartographer::EventContext::default(),
            serde_json::json!({
                "agent": self.agent,
                "model": options.model,
                "prompt_len": prompt.len(),
                "prompt_hash": short_prompt_hash(prompt.as_bytes()),
            }),
        );
        let started = std::time::Instant::now();
        let result = llm_client::run_agent(
            &provider,
            &system_prompt,
            prompt,
            &tools,
            timeout,
            |name, input| dispatch_tool(workspace, name, input),
        );
        let elapsed = started.elapsed().as_secs_f64();
        let result = match result {
            Ok(r) => {
                crate::cartographer::emit(
                    "llm-invoke",
                    "done",
                    "info",
                    crate::cartographer::EventContext::default(),
                    serde_json::json!({
                        "agent": self.agent,
                        "elapsed_s": elapsed,
                        "tokens_in": r.tokens_in,
                        "tokens_out": r.tokens_out,
                    }),
                );
                r
            }
            Err(e) => {
                crate::cartographer::emit(
                    "llm-invoke",
                    "error",
                    "warning",
                    crate::cartographer::EventContext::default(),
                    serde_json::json!({"agent": self.agent, "elapsed_s": elapsed, "error": e.to_string()}),
                );
                return Err(e.into());
            }
        };

        Ok(BackendOutcome {
            summary: result.text,
            tokens_in: result.tokens_in,
            tokens_out: result.tokens_out,
            // RAL-326: this tool loop never attaches `cache_control` to any
            // content block, so Anthropic writes no cache entry and reports
            // both cache tiers as zero; Ollama's OpenAI-shaped `usage` has no
            // cache breakdown at all. Nothing to read on either path.
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            agent_session_id: None,
            abandoned_background_job: None,
            // RAL-339: the hand-rolled tool loop has no compaction concept
            // of its own -- it never resumes a backend-native session.
            compaction_thrash: None,
            // RAL-373: this backend reports no compaction data, not "never
            // compacts".
            compaction_input_tokens: 0,
            compaction_count: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_prompt_hash_matches_known_vector_prefix() {
        assert_eq!(short_prompt_hash(b"abc"), "ba7816bf");
    }

    #[test]
    fn dispatch_tool_read_write_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("ralphus-agent-backend-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(&dir).unwrap();
        let out = dispatch_tool(
            &ws,
            "write_file",
            &json!({"path": "a.txt", "content": "hi"}),
        );
        assert_eq!(out, "ok");
        let out = dispatch_tool(&ws, "read_file", &json!({"path": "a.txt"}));
        assert_eq!(out, "hi");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dispatch_tool_reports_errors_as_strings_not_panics() {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-agent-backend-test-err-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::create(&dir).unwrap();
        let out = dispatch_tool(&ws, "read_file", &json!({"path": "missing.txt"}));
        assert!(out.starts_with("error:"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
