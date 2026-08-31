//! A minimal MCP stdio transport: newline-delimited JSON-RPC 2.0 over
//! stdin/stdout (no `Content-Length` framing -- that's LSP's convention, not
//! MCP's stdio transport). Hand-rolled rather than pulling in an MCP SDK
//! crate, matching this workspace's existing house style of hand-rolling its
//! own thin protocol layers over `ureq`/`tiny_http` instead of adopting a
//! framework (see `daemon/src/server.rs`).
//!
//! stdout here is a wire contract, not a log -- exactly the same invariant
//! `runner/`'s daemon<->runner JSON channel and this crate's own
//! `[workspace.lints.clippy] print_stdout = "deny"` protect elsewhere. Only
//! [`write_message`] may write to stdout; nothing else in this crate should.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::server::Server;

/// Writes one JSON-RPC message as a single line, per MCP's stdio transport.
/// The one sanctioned stdout writer in this crate -- see this module's doc
/// comment.
#[allow(clippy::print_stdout)]
fn write_message(out: &mut impl Write, message: &Value) -> std::io::Result<()> {
    writeln!(out, "{message}")?;
    out.flush()
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn success_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Handles one already-parsed JSON-RPC request/notification, returning the
/// response to write (`None` for a notification, which gets no reply -- MCP
/// notifications are identified by having no `id` field at all).
fn handle(server: &Server, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    let Some(id) = id else {
        // A notification (e.g. `notifications/initialized`) -- MCP servers
        // don't reply to these.
        return None;
    };

    let result = match method {
        "initialize" => Ok(server.initialize_result()),
        "tools/list" => Ok(server.tools_list_result()),
        "tools/call" => server.tools_call_result(&params),
        "ping" => Ok(json!({})),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match result {
        Ok(value) => success_response(id, value),
        Err((code, message)) => error_response(id, code, &message),
    })
}

/// Runs the stdio read-eval-respond loop until stdin closes. Requests are
/// handled strictly one at a time (no concurrent `tools/call`s in flight),
/// which keeps this crate's tool-execution layer free of any need for
/// thread-safety.
pub fn serve(server: &Server, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(trimmed) {
            Ok(request) => handle(server, &request),
            Err(e) => Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        if let Some(response) = response {
            write_message(&mut output, &response)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server() -> Server {
        Server::new("http://127.0.0.1:1".to_string(), false)
    }

    #[test]
    fn initialize_returns_server_info() {
        let server = test_server();
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        let resp = handle(&server, &req).expect("response");
        assert_eq!(resp["id"], json!(1));
        assert!(resp["result"]["serverInfo"]["name"].is_string());
    }

    #[test]
    fn notification_gets_no_response() {
        let server = test_server();
        let req = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle(&server, &req).is_none());
    }

    #[test]
    fn unknown_method_is_an_error_response() {
        let server = test_server();
        let req = json!({"jsonrpc": "2.0", "id": 2, "method": "bogus/method"});
        let resp = handle(&server, &req).expect("response");
        assert_eq!(resp["error"]["code"], json!(-32601));
    }

    #[test]
    fn tools_list_returns_a_nonempty_array() {
        let server = test_server();
        let req = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"});
        let resp = handle(&server, &req).expect("response");
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        assert!(!tools.is_empty());
    }

    #[test]
    fn serve_processes_one_line_and_writes_one_response() {
        let server = test_server();
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n" as &[u8];
        let mut output = Vec::new();
        serve(&server, input, &mut output).expect("serve");
        let text = String::from_utf8(output).expect("utf8");
        assert_eq!(text.lines().count(), 1);
        let parsed: Value = serde_json::from_str(text.lines().next().unwrap()).expect("json");
        assert_eq!(parsed["result"], json!({}));
    }
}
