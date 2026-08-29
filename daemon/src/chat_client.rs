//! Direct LLM API calls for the Guardian feedback chat.
//!
//! Bypasses the Python subprocess runner entirely for the `claude`/`anthropic`
//! and `ollama` backends — the two backends used for all real-world chat — by
//! calling the provider HTTP APIs directly with `ureq`. This eliminates the
//! 1–3 s Python cold-start (interpreter launch + pydantic-ai imports) that was
//! the dominant per-message latency cost.
//!
//! The chat triage role is conversational: the Guardian should reply with
//! routing notes, not edit files. Bypassing pydantic-ai's agentic tool loop is
//! therefore appropriate and also avoids spurious file-read rounds that inflated
//! token counts.

use serde_json::json;

/// One turn in a conversation, using Claude/OpenAI API role names.
pub struct ChatMessage {
    /// `"user"` for reviewer turns, `"assistant"` for guardian turns.
    pub role: &'static str,
    /// Message body.
    pub content: String,
    /// Optional base64 data-URI image attached to this message (RAL-59).
    pub image: Option<String>,
}

/// Dispatch a chat call to the appropriate provider without spawning a
/// subprocess. Returns the model's reply text on success, or `Err(reason)`
/// when the backend is unsupported or the provider call fails.
///
/// Supported backends: `"claude"` / `"anthropic"` (Anthropic Messages API) and
/// `"ollama"` (Ollama OpenAI-compatible endpoint). Any other agent string falls
/// back to `Err` so the caller can use the subprocess runner instead.
pub fn call_direct(
    agent: &str,
    model: Option<&str>,
    system: &str,
    messages: &[ChatMessage],
) -> Result<String, String> {
    let msg_count = messages.len();
    // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
    crate::rlog!(
        DEBUG,
        "ralphus [guardian] chat-api start backend={agent:?} model={model:?} messages={msg_count}"
    );
    let result = match agent.to_lowercase().as_str() {
        "claude" | "anthropic" => {
            call_claude(model.unwrap_or("claude-haiku-4-5"), system, messages)
        }
        "ollama" => {
            let base_url = std::env::var("RALPHUS_OLLAMA_URL")
                .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
            call_ollama(&base_url, model.unwrap_or("qwen3:8b"), system, messages)
        }
        other => Err(format!(
            "agent '{other}' is not supported for direct chat; use the subprocess runner"
        )),
    };
    match &result {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        Ok(_) => crate::rlog!(DEBUG, "ralphus [guardian] chat-api done backend={agent:?}"),
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        Err(e) => crate::rlog!(
            ERROR,
            "ralphus [guardian] chat-api error backend={agent:?}: {e}"
        ),
    }
    result
}

/// Parse a `data:image/...;base64,...` URI into `(media_type, base64_data)`.
/// Falls back to `("image/jpeg", uri)` for unrecognised formats.
fn parse_data_uri(uri: &str) -> (&str, &str) {
    if let Some(rest) = uri.strip_prefix("data:") {
        if let Some(comma) = rest.find(',') {
            let meta = &rest[..comma];
            let data = &rest[comma + 1..];
            let media_type = meta.split(';').next().unwrap_or("image/jpeg");
            return (media_type, data);
        }
    }
    ("image/jpeg", uri)
}

/// POST to the Anthropic Claude Messages API and return the first text block.
fn call_claude(model: &str, system: &str, messages: &[ChatMessage]) -> Result<String, String> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY is not set".to_string())?;

    let msgs_json: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            if let Some(img) = &m.image {
                let (media_type, data) = parse_data_uri(img);
                json!({
                    "role": m.role,
                    "content": [
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data
                            }
                        },
                        {"type": "text", "text": m.content}
                    ]
                })
            } else {
                json!({"role": m.role, "content": m.content})
            }
        })
        .collect();

    let payload = json!({
        "model": model,
        "max_tokens": 1024,
        "system": system,
        "messages": msgs_json,
    })
    .to_string();

    let response = ureq::post("https://api.anthropic.com/v1/messages")
        .set("x-api-key", &api_key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_string(&payload)
        .map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                format!("Claude API {code}: {body}")
            }
            other => format!("Claude API: {other}"),
        })?;

    let body = response
        .into_string()
        .map_err(|e| format!("Claude API read: {e}"))?;

    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Claude API JSON parse: {e}"))?;

    v["content"]
        .as_array()
        .and_then(|arr| arr.iter().find(|c| c["type"] == "text"))
        .and_then(|c| c["text"].as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("unexpected Claude response shape: {body}"))
}

/// POST to Ollama's OpenAI-compatible `/v1/chat/completions` endpoint.
fn call_ollama(
    base_url: &str,
    model: &str,
    system: &str,
    messages: &[ChatMessage],
) -> Result<String, String> {
    let mut all_msgs: Vec<serde_json::Value> = Vec::with_capacity(messages.len() + 1);
    all_msgs.push(json!({"role": "system", "content": system}));
    all_msgs.extend(messages.iter().map(|m| {
        if let Some(img) = &m.image {
            json!({
                "role": m.role,
                "content": [
                    {"type": "text", "text": m.content},
                    {"type": "image_url", "image_url": {"url": img}}
                ]
            })
        } else {
            json!({"role": m.role, "content": m.content})
        }
    }));

    let payload = json!({
        "model": model,
        "messages": all_msgs,
    })
    .to_string();

    let url = format!("{base_url}/chat/completions");
    let response = ureq::post(&url)
        .set("content-type", "application/json")
        .send_string(&payload)
        .map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                format!("Ollama API {code}: {body}")
            }
            other => format!("Ollama API: {other}"),
        })?;

    let body = response
        .into_string()
        .map_err(|e| format!("Ollama API read: {e}"))?;

    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Ollama API JSON parse: {e}"))?;

    v["choices"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|c| c["message"]["content"].as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("unexpected Ollama response shape: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_agent_returns_err() {
        let msgs = [ChatMessage {
            role: "user",
            content: "hi".to_string(),
            image: None,
        }];
        let err = call_direct("claude-code", None, "system", &msgs).unwrap_err();
        assert!(err.contains("not supported"), "got: {err}");
    }

    #[test]
    fn claude_without_api_key_returns_err() {
        // Only run when the key is absent so we never make a live HTTP call in CI.
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            return;
        }
        let msgs = [ChatMessage {
            role: "user",
            content: "hi".to_string(),
            image: None,
        }];
        let err = call_direct("claude", None, "system", &msgs).unwrap_err();
        assert!(
            err.contains("ANTHROPIC_API_KEY"),
            "expected key-not-set error, got: {err}"
        );
    }

    #[test]
    fn anthropic_alias_routes_to_claude() {
        // "anthropic" is an alias for "claude" — should not hit "not supported".
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            return;
        }
        let msgs = [ChatMessage {
            role: "user",
            content: "hi".to_string(),
            image: None,
        }];
        let err = call_direct("anthropic", None, "system", &msgs).unwrap_err();
        assert!(!err.contains("not supported"), "got: {err}");
    }
}
