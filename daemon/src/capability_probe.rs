//! RAL-695: checking and importing catalog items, on the daemon host only.
//!
//! * [`probe_mcp`] starts an MCP server (or calls its URL), performs the
//!   `initialize` + `tools/list` handshake and stops it. Nothing here touches
//!   the store: callers run it with no lock held and record the outcome with
//!   `Store::set_capability_status`.
//! * [`check_skill`] reports whether a skill folder and its `SKILL.md` exist.
//! * [`discover_from_home`] reads the user's own CLI configs (the Claude Code,
//!   Codex and Pi MCP files and the skill folders) into catalog candidates.
//!   Reading happens only when the user asks for an import; spawned agents
//!   never see those files.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agent_profile_env::{AgentEnvEntry, AgentEnvKind};
use crate::capability_store::{
    CapabilityKind, McpConfig, SkillConfig, check_skill_folder, resolved_env,
};

/// How long one handshake step may take.
pub const PROBE_STEP_TIMEOUT: Duration = Duration::from_secs(20);

const PROTOCOL_VERSION: &str = "2024-11-05";
const STDERR_TAIL_BYTES: usize = 600;
/// How long to wait for the stderr pipe to reach EOF after the server exits,
/// so a server that writes its error and quits still has it in the detail.
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The result of probing one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    pub ok: bool,
    pub tool_count: Option<i64>,
    /// The tool names `tools/list` returned, sorted and de-duplicated; `None`
    /// when the probe failed (the earlier inventory is then kept).
    pub tools: Option<Vec<String>>,
    /// The failure reason; redacted against every registered secret.
    pub detail: Option<String>,
    /// What the server said about itself in its `initialize` reply: its
    /// `serverInfo` name/version and `instructions`; redacted like `detail`.
    pub server_info: Option<String>,
}

/// A one-line summary of an `initialize` reply's `serverInfo` and
/// `instructions`, `None` when the server reported neither.
fn describe_initialize(init: &Value) -> Option<String> {
    let result = init.get("result")?;
    let info = result.get("serverInfo");
    let field = |key: &str| {
        info.and_then(|i| i.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let ident = match (field("name"), field("version")) {
        (Some(n), Some(v)) => Some(format!("{n} {v}")),
        (Some(n), None) => Some(n.to_string()),
        (None, _) => None,
    };
    let instructions = result
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let text = match (ident, instructions) {
        (Some(i), Some(t)) => format!("{i}: {t}"),
        (Some(i), None) => i,
        (None, Some(t)) => t.to_string(),
        (None, None) => return None,
    };
    Some(text)
}

impl ProbeOutcome {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            tool_count: None,
            tools: None,
            detail: Some(message.into()),
            server_info: None,
        }
    }

    /// The outcome recorded when a probe thread panics.
    #[must_use]
    pub fn probe_panicked() -> Self {
        Self::failed("the probe crashed")
    }

    fn listed(mut names: Vec<String>, server_info: Option<String>) -> Self {
        names.sort();
        names.dedup();
        Self {
            ok: true,
            tool_count: Some(i64::try_from(names.len()).unwrap_or(i64::MAX)),
            tools: Some(names),
            detail: None,
            server_info,
        }
    }
}

/// Append the `name` of every tool in one `tools/list` result page.
fn collect_tool_names(result: Option<&Value>, out: &mut Vec<String>) {
    let tools = result
        .and_then(|r| r.get("tools"))
        .and_then(Value::as_array);
    out.extend(
        tools
            .into_iter()
            .flatten()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .map(str::to_string),
    );
}

/// Probe `config` and scrub the outcome of the server's own secret values.
#[must_use]
pub fn probe_mcp(config: &McpConfig) -> ProbeOutcome {
    let env = resolved_env(&config.env);
    let headers = resolved_env(&config.headers);
    crate::redact::register_all(env.values().cloned().chain(headers.values().cloned()));
    let mut outcome = if let Some(url) = config.url.as_deref().filter(|u| !u.trim().is_empty()) {
        probe_http(url, &headers)
    } else if let Some(command) = config.command.as_deref() {
        probe_stdio(command, &config.args, &env)
    } else {
        ProbeOutcome::failed("the server has neither a command nor a url")
    };
    outcome.detail = outcome.detail.map(|d| crate::redact::redact_all(&d));
    outcome.server_info = outcome.server_info.map(|d| crate::redact::redact_all(&d));
    outcome
}

/// `Ok` when the folder at `path` holds a `SKILL.md`.
///
/// # Errors
/// What is missing.
pub fn check_skill(path: &str) -> Result<(), String> {
    check_skill_folder(Path::new(path))
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "ralphus-probe", "version": env!("CARGO_PKG_VERSION")},
        },
    })
}

fn tools_request(id: u64, cursor: Option<&str>) -> Value {
    let params = cursor.map_or_else(|| json!({}), |c| json!({"cursor": c}));
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": params})
}

fn initialized_notification() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
}

fn rpc_error(response: &Value) -> Option<String> {
    response.get("error").map(|e| {
        e.get("message")
            .and_then(Value::as_str)
            .map_or_else(|| e.to_string(), str::to_string)
    })
}

/// Spawn `command`, retrying through `cmd /C` on Windows where a bare name
/// such as `npx` is a `.cmd` shim `CreateProcess` will not resolve.
fn spawn_server(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> std::io::Result<Child> {
    let build = |program: &str, leading: &[&str]| {
        let mut c = Command::new(program);
        c.args(leading)
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    };
    match build(command, &[]).spawn() {
        #[cfg(windows)]
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            build("cmd", &["/C", command]).spawn()
        }
        other => other,
    }
}

fn probe_stdio(command: &str, args: &[String], env: &BTreeMap<String, String>) -> ProbeOutcome {
    let mut child = match spawn_server(command, args, env) {
        Ok(child) => child,
        Err(e) => return ProbeOutcome::failed(format!("cannot start {command:?}: {e}")),
    };
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (Some(mut stdin), Some(stdout)) = (stdin, stdout) else {
        let _ = child.kill();
        let _ = child.wait();
        return ProbeOutcome::failed("cannot attach to the server's pipes");
    };

    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if tx.send(value).is_err() {
                break;
            }
        }
    });
    let tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let (stderr_done_tx, stderr_done_rx) = mpsc::channel::<()>();
    if let Some(mut stderr) = stderr {
        let tail = std::sync::Arc::clone(&tail);
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut t) = tail.lock() {
                    t.extend_from_slice(&buf[..n]);
                    let len = t.len();
                    if len > STDERR_TAIL_BYTES {
                        t.drain(..len - STDERR_TAIL_BYTES);
                    }
                }
            }
            let _ = stderr_done_tx.send(());
        });
    } else {
        drop(stderr_done_tx);
    }

    let exchange =
        |stdin: &mut std::process::ChildStdin, request: &Value, id: u64| -> Result<Value, String> {
            writeln!(stdin, "{request}")
                .and_then(|()| stdin.flush())
                .map_err(|e| format!("the server closed its input: {e}"))?;
            let deadline = Instant::now() + PROBE_STEP_TIMEOUT;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok(msg) if msg.get("id").and_then(Value::as_u64) == Some(id) => return Ok(msg),
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        return Err(format!(
                            "no reply within {} seconds",
                            PROBE_STEP_TIMEOUT.as_secs()
                        ));
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err("the server exited before replying".to_string());
                    }
                }
            }
        };

    let result = (|| {
        let init = exchange(&mut stdin, &initialize_request(), 1)?;
        if let Some(message) = rpc_error(&init) {
            return Err(format!("initialize failed: {message}"));
        }
        writeln!(stdin, "{}", initialized_notification())
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("the server closed its input: {e}"))?;
        let mut names = Vec::new();
        let mut cursor: Option<String> = None;
        for page in 0..20u64 {
            let reply = exchange(
                &mut stdin,
                &tools_request(2 + page, cursor.as_deref()),
                2 + page,
            )?;
            if let Some(message) = rpc_error(&reply) {
                return Err(format!("tools/list failed: {message}"));
            }
            let result = reply.get("result");
            collect_tool_names(result, &mut names);
            cursor = result
                .and_then(|r| r.get("nextCursor"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok((names, describe_initialize(&init)))
    })();

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    // The reader thread finishes once the pipe closes; wait for it so a
    // server's last words are in the tail before it is read below.
    let _ = stderr_done_rx.recv_timeout(STDERR_DRAIN_TIMEOUT);

    match result {
        Ok((names, info)) => ProbeOutcome::listed(names, info),
        Err(message) => {
            let stderr_tail = tail
                .lock()
                .map(|t| String::from_utf8_lossy(&t).trim().to_string())
                .unwrap_or_default();
            ProbeOutcome::failed(if stderr_tail.is_empty() {
                message
            } else {
                format!("{message}; stderr: {stderr_tail}")
            })
        }
    }
}

/// The JSON-RPC message in a streamable-HTTP reply body: plain JSON or the
/// last `data:` event of an event stream.
fn parse_rpc_body(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
        return Some(value);
    }
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .next_back()
}

fn probe_http(url: &str, headers: &BTreeMap<String, String>) -> ProbeOutcome {
    let agent = ureq::AgentBuilder::new()
        .timeout(PROBE_STEP_TIMEOUT)
        .build();
    let mut session: Option<String> = None;
    let mut post = |body: &Value, expect_reply: bool| -> Result<Option<Value>, String> {
        let mut req = agent
            .post(url)
            .set("content-type", "application/json")
            .set("accept", "application/json, text/event-stream");
        for (k, v) in headers {
            req = req.set(k, v);
        }
        if let Some(id) = &session {
            req = req.set("mcp-session-id", id);
        }
        let resp = req
            .send_string(&body.to_string())
            .map_err(|e| format!("request failed: {e}"))?;
        if let Some(id) = resp.header("mcp-session-id") {
            session = Some(id.to_string());
        }
        let text = resp.into_string().map_err(|e| format!("bad reply: {e}"))?;
        if !expect_reply {
            return Ok(None);
        }
        parse_rpc_body(&text)
            .map(Some)
            .ok_or_else(|| "the reply was not a JSON-RPC message".to_string())
    };
    let result = (|| {
        let init = post(&initialize_request(), true)?.unwrap_or(Value::Null);
        if let Some(message) = rpc_error(&init) {
            return Err(format!("initialize failed: {message}"));
        }
        post(&initialized_notification(), false)?;
        let mut names = Vec::new();
        let mut cursor: Option<String> = None;
        for page in 0..20u64 {
            let reply =
                post(&tools_request(2 + page, cursor.as_deref()), true)?.unwrap_or(Value::Null);
            if let Some(message) = rpc_error(&reply) {
                return Err(format!("tools/list failed: {message}"));
            }
            let result = reply.get("result");
            collect_tool_names(result, &mut names);
            cursor = result
                .and_then(|r| r.get("nextCursor"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok((names, describe_initialize(&init)))
    })();
    match result {
        Ok((names, info)) => ProbeOutcome::listed(names, info),
        Err(message) => ProbeOutcome::failed(message),
    }
}

/// One entry an import found.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportCandidate {
    pub kind: CapabilityKind,
    pub name: String,
    /// `claude-code`, `codex`, `pi` or the skill folder's owner.
    pub origin: &'static str,
    pub mcp: Option<McpConfig>,
    pub skill: Option<SkillConfig>,
}

fn set_entries(value: Option<&Value>) -> Vec<AgentEnvEntry> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(k, v)| {
            v.as_str().map(|s| AgentEnvEntry {
                key: k.clone(),
                kind: AgentEnvKind::Set,
                value: s.to_string(),
            })
        })
        .collect()
}

fn json_server(value: &Value) -> Option<McpConfig> {
    let obj = value.as_object()?;
    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string);
    let url = obj.get("url").and_then(Value::as_str).map(str::to_string);
    if command.is_some() == url.is_some() {
        return None;
    }
    Some(McpConfig {
        command,
        args: obj
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        env: set_entries(obj.get("env")),
        url,
        headers: set_entries(obj.get("headers")),
        description: None,
    })
}

fn json_servers(text: &str, origin: &'static str, out: &mut Vec<ImportCandidate>) {
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let Some(servers) = root.get("mcpServers").and_then(Value::as_object) else {
        return;
    };
    for (name, value) in servers {
        if let Some(mcp) = json_server(value) {
            out.push(ImportCandidate {
                kind: CapabilityKind::Mcp,
                name: name.clone(),
                origin,
                mcp: Some(mcp),
                skill: None,
            });
        }
    }
}

fn codex_servers(text: &str, out: &mut Vec<ImportCandidate>) {
    let Ok(root) = toml::from_str::<toml::Table>(text) else {
        return;
    };
    let Some(servers) = root.get("mcp_servers").and_then(toml::Value::as_table) else {
        return;
    };
    for (name, value) in servers {
        let Some(table) = value.as_table() else {
            continue;
        };
        let strings = |key: &str| -> Option<Value> {
            table.get(key).and_then(toml::Value::as_table).map(|t| {
                Value::Object(
                    t.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), json!(s))))
                        .collect(),
                )
            })
        };
        let mut obj = serde_json::Map::new();
        if let Some(c) = table.get("command").and_then(toml::Value::as_str) {
            obj.insert("command".to_string(), json!(c));
        }
        if let Some(args) = table.get("args").and_then(toml::Value::as_array) {
            let args: Vec<&str> = args.iter().filter_map(toml::Value::as_str).collect();
            obj.insert("args".to_string(), json!(args));
        }
        if let Some(url) = table.get("url").and_then(toml::Value::as_str) {
            obj.insert("url".to_string(), json!(url));
        }
        if let Some(env) = strings("env") {
            obj.insert("env".to_string(), env);
        }
        if let Some(headers) = strings("http_headers") {
            obj.insert("headers".to_string(), headers);
        }
        if let Some(mcp) = json_server(&Value::Object(obj)) {
            out.push(ImportCandidate {
                kind: CapabilityKind::Mcp,
                name: name.clone(),
                origin: "codex",
                mcp: Some(mcp),
                skill: None,
            });
        }
    }
}

fn skill_folders(root: &Path, origin: &'static str, out: &mut Vec<ImportCandidate>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("SKILL.md").is_file())
        .collect();
    dirs.sort();
    for dir in dirs {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        out.push(ImportCandidate {
            kind: CapabilityKind::Skill,
            name: name.to_string(),
            origin,
            mcp: None,
            skill: Some(SkillConfig {
                path: dir.to_string_lossy().into_owned(),
            }),
        });
    }
}

/// Everything importable from the CLI configs under `home`. A source that is
/// missing or unparsable contributes nothing; the first source to name an
/// item wins.
#[must_use]
pub fn discover_from_home(home: &Path) -> Vec<ImportCandidate> {
    let mut found = Vec::new();
    if let Ok(text) = std::fs::read_to_string(home.join(".claude.json")) {
        json_servers(&text, "claude-code", &mut found);
    }
    if let Ok(text) = std::fs::read_to_string(home.join(".codex").join("config.toml")) {
        codex_servers(&text, &mut found);
    }
    if let Ok(text) = std::fs::read_to_string(home.join(".config").join("mcp").join("mcp.json")) {
        json_servers(&text, "pi", &mut found);
    }
    skill_folders(
        &home.join(".claude").join("skills"),
        "claude-code",
        &mut found,
    );
    skill_folders(&home.join(".codex").join("skills"), "codex", &mut found);
    skill_folders(
        &home.join(".pi").join("agent").join("skills"),
        "pi",
        &mut found,
    );
    let mut seen = std::collections::BTreeSet::new();
    found.retain(|c| seen.insert((c.kind.as_str(), c.name.clone())));
    found
}

/// The directory import reads from: the daemon user's home.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_initialize_combines_server_info_and_instructions() {
        let both = serde_json::json!({"result":{"serverInfo":{"name":"srv","version":"1.2"},"instructions":" Search docs. "}});
        assert_eq!(
            describe_initialize(&both).as_deref(),
            Some("srv 1.2: Search docs.")
        );
        let name_only = serde_json::json!({"result":{"serverInfo":{"name":"srv"}}});
        assert_eq!(describe_initialize(&name_only).as_deref(), Some("srv"));
        let instructions_only = serde_json::json!({"result":{"instructions":"Do X"}});
        assert_eq!(
            describe_initialize(&instructions_only).as_deref(),
            Some("Do X")
        );
        assert_eq!(describe_initialize(&serde_json::json!({"result":{}})), None);
        assert_eq!(describe_initialize(&serde_json::json!({})), None);
    }

    fn temp_home(tag: &str) -> PathBuf {
        let dir = ralphus_test_support::scratch(format!(
            "ral695-home-{tag}-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_servers_and_skills_from_every_source() {
        let home = temp_home("discover");
        std::fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","fs"],"env":{"K":"v"}},
                "web":{"type":"http","url":"https://x.test/mcp","headers":{"Authorization":"Bearer t"}},
                "broken":{"args":[]}}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex").join("config.toml"),
            "[mcp_servers.git]\ncommand = \"git-mcp\"\nargs = [\"--ro\"]\n[mcp_servers.git.env]\nTOKEN = \"abc\"\n[mcp_servers.fs]\ncommand = \"other\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".config").join("mcp")).unwrap();
        std::fs::write(
            home.join(".config").join("mcp").join("mcp.json"),
            r#"{"mcpServers":{"pi-only":{"command":"pi-srv"}}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".claude").join("skills").join("alpha")).unwrap();
        std::fs::write(
            home.join(".claude")
                .join("skills")
                .join("alpha")
                .join("SKILL.md"),
            "x",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".claude").join("skills").join("nofile")).unwrap();

        let found = discover_from_home(&home);
        let names: Vec<(&str, &str)> = found
            .iter()
            .map(|c| (c.kind.as_str(), c.name.as_str()))
            .collect();
        assert!(names.contains(&("mcp", "fs")));
        assert!(names.contains(&("mcp", "web")));
        assert!(names.contains(&("mcp", "git")));
        assert!(names.contains(&("mcp", "pi-only")));
        assert!(names.contains(&("skill", "alpha")));
        assert!(!names.contains(&("mcp", "broken")));
        assert!(!names.contains(&("skill", "nofile")));
        // The Claude Code file came first, so its `fs` wins over Codex's.
        let fs = found.iter().find(|c| c.name == "fs").unwrap();
        assert_eq!(fs.origin, "claude-code");
        let git = found.iter().find(|c| c.name == "git").unwrap();
        assert_eq!(git.mcp.as_ref().unwrap().env[0].key, "TOKEN");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_home_sources_yield_nothing() {
        let home = temp_home("empty");
        assert!(discover_from_home(&home).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn rpc_body_parses_json_and_event_streams() {
        assert_eq!(parse_rpc_body(r#"{"id":1}"#).unwrap(), json!({"id": 1}));
        let sse = "event: message\ndata: {\"id\":2,\"result\":{}}\n\n";
        assert_eq!(parse_rpc_body(sse).unwrap()["id"], 2);
        assert!(parse_rpc_body("nope").is_none());
    }

    #[test]
    fn a_missing_command_is_a_failing_probe() {
        let outcome = probe_mcp(&McpConfig {
            command: Some("ralphus-no-such-mcp-server-695".to_string()),
            ..McpConfig::default()
        });
        assert!(!outcome.ok);
        assert!(outcome.detail.is_some());
    }

    #[test]
    fn probe_failure_text_is_redacted() {
        crate::redact::with_registry_lock(|| {
            let outcome = probe_mcp(&McpConfig {
                command: Some("ralphus-no-such-mcp-server-695".to_string()),
                args: vec![],
                env: vec![AgentEnvEntry {
                    key: "T".to_string(),
                    kind: AgentEnvKind::Set,
                    value: "ralphus-no-such".to_string(),
                }],
                ..McpConfig::default()
            });
            assert!(!outcome.detail.unwrap().contains("ralphus-no-such"));
        });
    }

    /// Spawns a real child process: tagged `mcp-probe` in `proc_counts/tags.toml`.
    #[cfg(unix)]
    #[test]
    fn probe_stdio_counts_tools_from_a_real_server() {
        let script = r#"read l; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"f","version":"1"}}}'; read l; read l; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"a"},{"name":"b"}]}}'"#;
        let outcome = probe_mcp(&McpConfig {
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), script.to_string()],
            ..McpConfig::default()
        });
        assert_eq!(outcome.detail, None);
        assert!(outcome.ok);
        assert_eq!(outcome.tool_count, Some(2));
        assert_eq!(outcome.tools, Some(vec!["a".to_string(), "b".to_string()]));
    }

    #[cfg(unix)]
    #[test]
    fn probe_reports_a_server_that_exits_silently() {
        let outcome = probe_mcp(&McpConfig {
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), "echo boom >&2".to_string()],
            ..McpConfig::default()
        });
        assert!(!outcome.ok);
        assert!(outcome.detail.unwrap().contains("boom"));
    }
}
