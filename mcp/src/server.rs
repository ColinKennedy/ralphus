//! Wires the [`crate::tools`] registry and [`crate::exec`] execution layer
//! into MCP's `initialize`/`tools/list`/`tools/call` shapes.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands;
use serde_json::{Map, Value, json};

use crate::exec;
use crate::tools::{self, Tool};

pub struct Server {
    daemon_url: String,
    read_only: bool,
    tools: Vec<Tool>,
}

impl Server {
    #[must_use]
    pub fn new(daemon_url: String, read_only: bool) -> Self {
        Self {
            daemon_url,
            read_only,
            tools: tools::all_tools(),
        }
    }

    fn visible_tools(&self) -> impl Iterator<Item = &Tool> {
        self.tools
            .iter()
            .filter(move |t| !self.read_only || t.read_only)
    }

    #[must_use]
    pub fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "ralphus-mcp", "version": env!("CARGO_PKG_VERSION")},
        })
    }

    #[must_use]
    pub fn tools_list_result(&self) -> Value {
        let tools: Vec<Value> = self
            .visible_tools()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect();
        json!({"tools": tools})
    }

    /// Handles `tools/call`. Returns `Err((code, message))` only for
    /// protocol-shaped problems (unknown tool, malformed params) -- a
    /// domain-level failure (bad selector, daemon rejected the request)
    /// comes back as `Ok` with `isError: true`, per the MCP tool-result
    /// convention (the call itself succeeded; the tool's own outcome did
    /// not).
    pub fn tools_call_result(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "missing 'name'".to_string()))?;
        let empty = Map::new();
        let arguments = params
            .get("arguments")
            .and_then(Value::as_object)
            .unwrap_or(&empty);

        let Some(tool) = self.visible_tools().find(|t| t.name == name) else {
            return Err((-32602, format!("unknown tool: {name}")));
        };

        let argv = match tool.build_argv(arguments) {
            Ok(argv) => argv,
            Err(message) => return Ok(tool_error(message)),
        };
        let cmd = commands::parse_args(&argv);
        let client = DaemonClient::new(self.daemon_url.clone());
        match exec::execute(cmd, &client) {
            Ok(value) => Ok(json!({
                "content": [{"type": "text", "text": render(&value)}],
                "isError": false,
            })),
            Err(e) => Ok(tool_error(exec::message(&e))),
        }
    }
}

fn render(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": message.into()}],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_call_reports_unknown_tool_as_protocol_error() {
        let server = Server::new("http://127.0.0.1:1".to_string(), false);
        let err = server
            .tools_call_result(&json!({"name": "bogus_tool"}))
            .unwrap_err();
        assert_eq!(err.0, -32602);
    }

    #[test]
    fn tools_call_reports_daemon_failure_as_tool_error_not_protocol_error() {
        // Port 1 is (almost certainly) refused immediately, giving a
        // deterministic "unreachable" DaemonError without a live fixture.
        let server = Server::new("http://127.0.0.1:1".to_string(), false);
        let result = server
            .tools_call_result(&json!({"name": "resources", "arguments": {}}))
            .expect("protocol-level success");
        assert_eq!(result["isError"], json!(true));
    }

    #[test]
    fn read_only_server_hides_mutating_tools_from_calls() {
        let server = Server::new("http://127.0.0.1:1".to_string(), true);
        let err = server
            .tools_call_result(&json!({"name": "task_set_status", "arguments": {}}))
            .unwrap_err();
        assert_eq!(err.0, -32602);
    }

    #[test]
    fn read_only_server_still_lists_read_only_tools() {
        let server = Server::new("http://127.0.0.1:1".to_string(), true);
        let tools = server.tools_list_result();
        let names: Vec<&str> = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"task_show"));
        assert!(!names.contains(&"task_set_status"));
    }
}
