//! Hand-rolled Anthropic Messages API + Ollama OpenAI-compatible chat client.
//!
//! This is the "genuine hand-roll" piece `PYTHON_DEPRECATION.local.md` (Part 2)
//! calls out: neither `cli/src/ralphus/author/agent.py` nor
//! `cli/src/ralphus/runner/pydantic_backend.py` used any pydantic *validation*
//! feature -- both just called `agent.run_sync(...)` and read `.output`. What
//! they needed from pydantic-ai was (a) picking Anthropic vs an
//! OpenAI-compatible endpoint (see [`crate::providers`]) and (b), for the
//! runner only, a tool-calling loop. Both providers' wire protocols are
//! implemented directly over `ureq` (already a workspace dependency; no
//! `reqwest`/`tokio`, matching this workspace's synchronous-by-design rule).

use std::time::Duration;

use serde_json::{Value, json};

use crate::providers::Provider;

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;
/// No cap is documented in the Python pydantic-ai tool loop (it hides
/// whatever internal default the library used); an explicit cap here is
/// cheap insurance against a runaway back-and-forth burning the whole session
/// budget on tool calls alone.
const MAX_TOOL_TURNS: u32 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmError(pub String);

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for LlmError {}

impl From<LlmError> for crate::backend::BackendError {
    fn from(e: LlmError) -> Self {
        crate::backend::BackendError(e.0)
    }
}

/// One tool definition, described as JSON Schema (only three tools ever
/// exist -- `read_file`/`write_file`/`run_bash` -- so these are built by hand
/// in [`crate::agent_backend`] rather than derived via a schema-generation
/// crate).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// The result of a plain (no-tools) single-turn generation. Mirrors Python's
/// `author.agent.GenerateResult`.
#[derive(Debug, Clone)]
pub struct GenerateResult {
    pub text: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
}

/// One tool call the model asked to make, and the result to append back.
struct ToolCall {
    id: String,
    name: String,
    input: Value,
}

/// A single-turn (no tools) text generation -- the `author/agent.py` shape.
/// `budget_tokens` is a soft pre/post-flight cap: since neither Anthropic's
/// nor Ollama's non-streaming APIs support server-side token-limit
/// enforcement, this checks the actual usage *after* the call returns and
/// reports [`LlmError`] if it was exceeded (matching pydantic-ai's
/// `UsageLimitExceeded` behavior closely enough: the spend already happened,
/// but the caller still learns the budget was blown).
pub fn generate(
    provider: &Provider,
    system_prompt: &str,
    user_prompt: &str,
    budget_tokens: Option<u64>,
    timeout: Option<Duration>,
) -> Result<GenerateResult, LlmError> {
    let (text, tokens_in, tokens_out) = match provider {
        Provider::Anthropic { model } => {
            let messages = vec![json!({"role": "user", "content": user_prompt})];
            let resp = anthropic_request(model, system_prompt, &messages, &[], timeout)?;
            let text = anthropic_text(&resp);
            let (ti, to) = anthropic_usage(&resp);
            (text, ti, to)
        }
        Provider::Ollama { model, base_url } => {
            let messages = vec![
                json!({"role": "system", "content": system_prompt}),
                json!({"role": "user", "content": user_prompt}),
            ];
            let resp = ollama_request(base_url, model, &messages, &[], timeout)?;
            let text = ollama_text(&resp);
            let (ti, to) = ollama_usage(&resp);
            (text, ti, to)
        }
    };
    if let Some(cap) = budget_tokens {
        let used = (tokens_in + tokens_out).max(0) as u64;
        if used > cap {
            return Err(LlmError(format!(
                "usage limit exceeded: used {used} tokens, budget was {cap}"
            )));
        }
    }
    Ok(GenerateResult {
        text,
        tokens_in,
        tokens_out,
    })
}

/// Runs the tool-calling loop (the `pydantic_backend.py` shape): send the
/// prompt with `tools` attached; while the model asks for tool calls, dispatch
/// each through `call_tool` and send the results back; stop once a turn comes
/// back with no tool calls, or [`MAX_TOOL_TURNS`] is hit. Token usage
/// accumulates across every turn.
pub fn run_agent(
    provider: &Provider,
    system_prompt: &str,
    user_prompt: &str,
    tools: &[ToolSpec],
    timeout: Option<Duration>,
    mut call_tool: impl FnMut(&str, &Value) -> String,
) -> Result<GenerateResult, LlmError> {
    match provider {
        Provider::Anthropic { model } => run_anthropic_agent(
            model,
            system_prompt,
            user_prompt,
            tools,
            timeout,
            &mut call_tool,
        ),
        Provider::Ollama { model, base_url } => run_ollama_agent(
            base_url,
            model,
            system_prompt,
            user_prompt,
            tools,
            timeout,
            &mut call_tool,
        ),
    }
}

// ---- Anthropic ---------------------------------------------------------

fn anthropic_tools_json(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect()
}

fn anthropic_request(
    model: &str,
    system_prompt: &str,
    messages: &[Value],
    tools: &[ToolSpec],
    timeout: Option<Duration>,
) -> Result<Value, LlmError> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| LlmError("ANTHROPIC_API_KEY is not set".to_string()))?;
    let mut body = json!({
        "model": model,
        "max_tokens": DEFAULT_MAX_TOKENS,
        "system": system_prompt,
        "messages": messages,
    });
    if !tools.is_empty() {
        body["tools"] = json!(anthropic_tools_json(tools));
    }
    let mut req = ureq::post(ANTHROPIC_API_URL)
        .set("x-api-key", &api_key)
        .set("anthropic-version", ANTHROPIC_VERSION)
        .set("content-type", "application/json");
    if let Some(t) = timeout {
        req = req.timeout(t);
    }
    let resp = req.send_string(&body.to_string()).map_err(|e| match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            LlmError(format!("Anthropic API {code}: {body}"))
        }
        other => LlmError(format!("Anthropic request failed: {other}")),
    })?;
    let text = resp
        .into_string()
        .map_err(|e| LlmError(format!("Anthropic response read failed: {e}")))?;
    serde_json::from_str(&text)
        .map_err(|e| LlmError(format!("Anthropic response was not valid JSON: {e}")))
}

fn anthropic_text(resp: &Value) -> String {
    resp["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn anthropic_usage(resp: &Value) -> (i64, i64) {
    (
        resp["usage"]["input_tokens"].as_i64().unwrap_or(0),
        resp["usage"]["output_tokens"].as_i64().unwrap_or(0),
    )
}

fn anthropic_tool_calls(resp: &Value) -> Vec<ToolCall> {
    resp["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "tool_use")
        .filter_map(|b| {
            Some(ToolCall {
                id: b["id"].as_str()?.to_string(),
                name: b["name"].as_str()?.to_string(),
                input: b["input"].clone(),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn run_anthropic_agent(
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
    tools: &[ToolSpec],
    timeout: Option<Duration>,
    call_tool: &mut dyn FnMut(&str, &Value) -> String,
) -> Result<GenerateResult, LlmError> {
    let mut messages = vec![json!({"role": "user", "content": user_prompt})];
    let mut tokens_in = 0i64;
    let mut tokens_out = 0i64;

    for _ in 0..MAX_TOOL_TURNS {
        let resp = anthropic_request(model, system_prompt, &messages, tools, timeout)?;
        let (ti, to) = anthropic_usage(&resp);
        tokens_in += ti;
        tokens_out += to;

        let calls = anthropic_tool_calls(&resp);
        if calls.is_empty() {
            return Ok(GenerateResult {
                text: anthropic_text(&resp),
                tokens_in,
                tokens_out,
            });
        }

        messages.push(json!({"role": "assistant", "content": resp["content"].clone()}));
        let results: Vec<Value> = calls
            .iter()
            .map(|c| {
                let output = call_tool(&c.name, &c.input);
                json!({
                    "type": "tool_result",
                    "tool_use_id": c.id,
                    "content": output,
                })
            })
            .collect();
        messages.push(json!({"role": "user", "content": results}));
    }

    Err(LlmError(format!(
        "tool-calling loop did not terminate within {MAX_TOOL_TURNS} turns"
    )))
}

// ---- Ollama (OpenAI-compatible chat completions) -----------------------

fn ollama_tools_json(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                },
            })
        })
        .collect()
}

fn ollama_request(
    base_url: &str,
    model: &str,
    messages: &[Value],
    tools: &[ToolSpec],
    timeout: Option<Duration>,
) -> Result<Value, LlmError> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut body = json!({
        "model": model,
        "messages": messages,
    });
    if !tools.is_empty() {
        body["tools"] = json!(ollama_tools_json(tools));
    }
    let mut req = ureq::post(&url).set("content-type", "application/json");
    if let Some(t) = timeout {
        req = req.timeout(t);
    }
    let resp = req.send_string(&body.to_string()).map_err(|e| match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            LlmError(format!("Ollama API {code}: {body}"))
        }
        other => LlmError(format!("Ollama request failed: {other}")),
    })?;
    let text = resp
        .into_string()
        .map_err(|e| LlmError(format!("Ollama response read failed: {e}")))?;
    serde_json::from_str(&text)
        .map_err(|e| LlmError(format!("Ollama response was not valid JSON: {e}")))
}

fn ollama_text(resp: &Value) -> String {
    resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn ollama_usage(resp: &Value) -> (i64, i64) {
    (
        resp["usage"]["prompt_tokens"].as_i64().unwrap_or(0),
        resp["usage"]["completion_tokens"].as_i64().unwrap_or(0),
    )
}

fn ollama_tool_calls(resp: &Value) -> Vec<ToolCall> {
    resp["choices"][0]["message"]["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let id = c["id"].as_str().unwrap_or_default().to_string();
            let name = c["function"]["name"].as_str()?.to_string();
            let raw_args = c["function"]["arguments"].as_str().unwrap_or("{}");
            let input: Value = serde_json::from_str(raw_args).unwrap_or(json!({}));
            Some(ToolCall { id, name, input })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn run_ollama_agent(
    base_url: &str,
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
    tools: &[ToolSpec],
    timeout: Option<Duration>,
    call_tool: &mut dyn FnMut(&str, &Value) -> String,
) -> Result<GenerateResult, LlmError> {
    let mut messages = vec![
        json!({"role": "system", "content": system_prompt}),
        json!({"role": "user", "content": user_prompt}),
    ];
    let mut tokens_in = 0i64;
    let mut tokens_out = 0i64;

    for _ in 0..MAX_TOOL_TURNS {
        let resp = ollama_request(base_url, model, &messages, tools, timeout)?;
        let (ti, to) = ollama_usage(&resp);
        tokens_in += ti;
        tokens_out += to;

        let calls = ollama_tool_calls(&resp);
        if calls.is_empty() {
            return Ok(GenerateResult {
                text: ollama_text(&resp),
                tokens_in,
                tokens_out,
            });
        }

        messages.push(resp["choices"][0]["message"].clone());
        for c in &calls {
            let output = call_tool(&c.name, &c.input);
            messages.push(json!({
                "role": "tool",
                "tool_call_id": c.id,
                "content": output,
            }));
        }
    }

    Err(LlmError(format!(
        "tool-calling loop did not terminate within {MAX_TOOL_TURNS} turns"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_text_joins_text_blocks() {
        let resp = json!({"content": [{"type": "text", "text": "hello "}, {"type": "text", "text": "world"}]});
        assert_eq!(anthropic_text(&resp), "hello world");
    }

    #[test]
    fn anthropic_usage_reads_nested_fields() {
        let resp = json!({"usage": {"input_tokens": 10, "output_tokens": 20}});
        assert_eq!(anthropic_usage(&resp), (10, 20));
    }

    #[test]
    fn anthropic_tool_calls_extracts_tool_use_blocks() {
        let resp = json!({"content": [
            {"type": "text", "text": "let me check"},
            {"type": "tool_use", "id": "abc", "name": "read_file", "input": {"path": "x"}},
        ]});
        let calls = anthropic_tool_calls(&resp);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].id, "abc");
    }

    #[test]
    fn ollama_usage_reads_openai_field_names() {
        let resp = json!({"usage": {"prompt_tokens": 5, "completion_tokens": 7}});
        assert_eq!(ollama_usage(&resp), (5, 7));
    }

    #[test]
    fn ollama_text_reads_message_content() {
        let resp = json!({"choices": [{"message": {"content": "answer"}}]});
        assert_eq!(ollama_text(&resp), "answer");
    }

    #[test]
    fn ollama_tool_calls_parses_function_arguments_json_string() {
        let resp = json!({"choices": [{"message": {"tool_calls": [
            {"id": "1", "function": {"name": "run_bash", "arguments": "{\"command\":\"ls\"}"}}
        ]}}]});
        let calls = ollama_tool_calls(&resp);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_bash");
        assert_eq!(calls[0].input["command"], "ls");
    }
}
