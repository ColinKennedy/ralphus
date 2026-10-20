//! RAL-695: the MCP servers and skills a spawned agent is allowed to use.
//!
//! The daemon resolves an agent profile's enabled catalog items into a
//! [`CapabilitySet`] and ships it in the runner spec, so a remote machine
//! provisions locally from the spec alone. The set is the *only* source of MCP
//! servers and skills for the session: each backend's
//! [`ModelBackend::provision_capabilities`](crate::backend::ModelBackend::provision_capabilities)
//! writes the harness's own config files into the isolated config directory
//! and returns any extra launcher arguments.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use crate::backend::BackendError;

/// How a session's capabilities reach its command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    /// The backend's own provisioning (config files, flags, skill folders).
    #[default]
    BackendDefault,
    /// Write generic files and append these arguments to the command.
    /// `{mcp_config}` expands to the MCP JSON path and `{skills_dir}` to the
    /// skills folder.
    ExtraArgs(Vec<String>),
    /// Deliver nothing.
    None,
}

/// One MCP server, with its environment already resolved.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpServerSpec {
    pub name: String,
    /// Stdio transport: the program to launch.
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// HTTP transport: the endpoint.
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// Tool names (as the server reports them) this session must not be able
    /// to call. Empty means every tool the server exposes is allowed.
    pub denied_tools: Vec<String>,
}

/// One file inside a skill folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFileSpec {
    /// Path relative to the skill folder, `/`-separated.
    pub path: String,
    pub bytes: Vec<u8>,
}

/// One skill folder's contents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillSpec {
    pub name: String,
    pub files: Vec<SkillFileSpec>,
}

/// The capabilities enabled for one session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapabilitySet {
    pub mcp_servers: Vec<McpServerSpec>,
    pub skills: Vec<SkillSpec>,
    pub delivery: DeliveryMode,
}

/// What a backend's provisioning produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Provisioning {
    /// Arguments to append to the launched command.
    pub args: Vec<String>,
    /// A file holding resolved secrets that the caller should delete after the
    /// run.
    pub temporary_files: Vec<PathBuf>,
}

impl CapabilitySet {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mcp_servers.is_empty() && self.skills.is_empty()
    }

    /// Parse the wire object the daemon puts under `capabilities`.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "capabilities must be an object".to_string())?;
        let mut set = Self::default();
        if let Some(items) = obj.get("mcp_servers").filter(|v| !v.is_null()) {
            let items = items
                .as_array()
                .ok_or_else(|| "capabilities.mcp_servers must be an array".to_string())?;
            for item in items {
                set.mcp_servers.push(parse_server(item)?);
            }
        }
        if let Some(items) = obj.get("skills").filter(|v| !v.is_null()) {
            let items = items
                .as_array()
                .ok_or_else(|| "capabilities.skills must be an array".to_string())?;
            for item in items {
                set.skills.push(parse_skill(item)?);
            }
        }
        if let Some(delivery) = obj.get("delivery").filter(|v| !v.is_null()) {
            set.delivery = parse_delivery(delivery)?;
        }
        Ok(set)
    }

    /// Whether any server has a denied tool, which a delivery that cannot
    /// filter tools must refuse rather than ignore.
    #[must_use]
    pub fn has_denied_tools(&self) -> bool {
        self.mcp_servers.iter().any(|s| !s.denied_tools.is_empty())
    }

    /// The Claude Code deny patterns (`mcp__<server>__<tool>`) for every
    /// denied tool, comma-joined for one `--disallowedTools` value.
    #[must_use]
    pub fn claude_disallowed_tools(&self) -> Option<String> {
        let patterns: Vec<String> = self
            .mcp_servers
            .iter()
            .flat_map(|server| {
                server.denied_tools.iter().map(|tool| {
                    format!(
                        "mcp__{}__{}",
                        claude_tool_component(&server.name),
                        claude_tool_component(tool)
                    )
                })
            })
            .collect();
        (!patterns.is_empty()).then(|| patterns.join(","))
    }

    /// The `{"mcpServers": {...}}` document Claude Code's `--mcp-config` and
    /// the Pi MCP adapter both read.
    #[must_use]
    pub fn mcp_json(&self) -> Value {
        let mut servers = Map::new();
        for server in &self.mcp_servers {
            let mut entry = Map::new();
            if let Some(url) = &server.url {
                entry.insert("type".to_string(), json!("http"));
                entry.insert("url".to_string(), json!(url));
                if !server.headers.is_empty() {
                    entry.insert("headers".to_string(), json!(server.headers));
                }
            } else {
                if let Some(command) = &server.command {
                    entry.insert("command".to_string(), json!(command));
                }
                if !server.args.is_empty() {
                    entry.insert("args".to_string(), json!(server.args));
                }
                if !server.env.is_empty() {
                    entry.insert("env".to_string(), json!(server.env));
                }
            }
            servers.insert(server.name.clone(), Value::Object(entry));
        }
        json!({ "mcpServers": servers })
    }

    /// The Codex `mcp_servers` table.
    #[must_use]
    pub fn codex_mcp_table(&self) -> toml::Table {
        let mut table = toml::Table::new();
        for server in &self.mcp_servers {
            let mut entry = toml::Table::new();
            if let Some(url) = &server.url {
                entry.insert("url".to_string(), toml::Value::String(url.clone()));
                if !server.headers.is_empty() {
                    entry.insert(
                        "http_headers".to_string(),
                        toml::Value::Table(string_table(&server.headers)),
                    );
                }
            } else {
                if let Some(command) = &server.command {
                    entry.insert("command".to_string(), toml::Value::String(command.clone()));
                }
                if !server.args.is_empty() {
                    entry.insert(
                        "args".to_string(),
                        toml::Value::Array(
                            server
                                .args
                                .iter()
                                .map(|a| toml::Value::String(a.clone()))
                                .collect(),
                        ),
                    );
                }
                if !server.env.is_empty() {
                    entry.insert(
                        "env".to_string(),
                        toml::Value::Table(string_table(&server.env)),
                    );
                }
            }
            if !server.denied_tools.is_empty() {
                entry.insert(
                    "disabled_tools".to_string(),
                    toml::Value::Array(
                        server
                            .denied_tools
                            .iter()
                            .map(|t| toml::Value::String(t.clone()))
                            .collect(),
                    ),
                );
            }
            table.insert(server.name.clone(), toml::Value::Table(entry));
        }
        table
    }
}

/// Claude Code names an MCP tool `mcp__<server>__<tool>` after replacing every
/// character outside `[A-Za-z0-9_-]` with `_`.
fn claude_tool_component(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The error a delivery without per-tool filtering returns when a tool is
/// denied, so the denial is never silently dropped.
pub fn refuse_tool_filters(set: &CapabilitySet, backend: &str) -> Result<(), BackendError> {
    if set.has_denied_tools() {
        Err(BackendError(format!(
            "{backend} cannot filter individual MCP tools; allow every tool of each enabled server for this profile"
        )))
    } else {
        Ok(())
    }
}

fn string_table(map: &BTreeMap<String, String>) -> toml::Table {
    map.iter()
        .map(|(k, v)| (k.clone(), toml::Value::String(v.clone())))
        .collect()
}

fn string_map(value: Option<&Value>, what: &str) -> Result<BTreeMap<String, String>, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(BTreeMap::new());
    };
    let obj = value
        .as_object()
        .ok_or_else(|| format!("{what} must be an object"))?;
    obj.iter()
        .map(|(k, v)| {
            v.as_str()
                .map(|s| (k.clone(), s.to_string()))
                .ok_or_else(|| format!("{what}.{k} must be a string"))
        })
        .collect()
}

fn parse_server(value: &Value) -> Result<McpServerSpec, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "an MCP server entry must be an object".to_string())?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "an MCP server entry needs a name".to_string())?
        .to_string();
    let args = match obj.get("args").filter(|v| !v.is_null()) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("MCP server {name}: args must be strings"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(format!("MCP server {name}: args must be an array")),
        None => Vec::new(),
    };
    Ok(McpServerSpec {
        command: obj
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string),
        args,
        env: string_map(obj.get("env"), "env")?,
        url: obj.get("url").and_then(Value::as_str).map(str::to_string),
        headers: string_map(obj.get("headers"), "headers")?,
        denied_tools: match obj.get("denied_tools").filter(|v| !v.is_null()) {
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("MCP server {name}: denied_tools must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(format!("MCP server {name}: denied_tools must be an array")),
            None => Vec::new(),
        },
        name,
    })
}

fn parse_skill(value: &Value) -> Result<SkillSpec, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "a skill entry must be an object".to_string())?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "a skill entry needs a name".to_string())?
        .to_string();
    let mut files = Vec::new();
    if let Some(items) = obj.get("files").and_then(Value::as_array) {
        for item in items {
            let path = item
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("skill {name}: a file needs a path"))?
                .to_string();
            let encoded = item
                .get("content_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("skill {name}: file {path} needs content_base64"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|e| format!("skill {name}: file {path}: {e}"))?;
            files.push(SkillFileSpec { path, bytes });
        }
    }
    Ok(SkillSpec { name, files })
}

fn parse_delivery(value: &Value) -> Result<DeliveryMode, String> {
    let mode = value
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("backend_default");
    match mode {
        "backend_default" => Ok(DeliveryMode::BackendDefault),
        "none" => Ok(DeliveryMode::None),
        "extra_args" => {
            let args = match value.get("args") {
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| "delivery.args must be strings".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => Vec::new(),
            };
            Ok(DeliveryMode::ExtraArgs(args))
        }
        other => Err(format!("unknown capability delivery mode: {other}")),
    }
}

/// Whether `name` is safe to use as a single path component.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && !name.contains('\0')
}

/// A skill file's relative path as a safe `PathBuf`, or `None` when it escapes
/// the skill folder.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let candidate = Path::new(path);
    let mut out = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => out.push(part),
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Replace `skills_dir` with exactly the given skills.
pub fn write_skills(skills_dir: &Path, skills: &[SkillSpec]) -> Result<(), BackendError> {
    if skills_dir.exists() {
        std::fs::remove_dir_all(skills_dir).map_err(|e| {
            BackendError(format!(
                "could not reset skills directory {}: {e}",
                skills_dir.display()
            ))
        })?;
    }
    if skills.is_empty() {
        return Ok(());
    }
    for skill in skills {
        if !is_safe_component(&skill.name) {
            return Err(BackendError(format!("unsafe skill name: {}", skill.name)));
        }
        let root = skills_dir.join(&skill.name);
        std::fs::create_dir_all(&root)
            .map_err(|e| BackendError(format!("could not create {}: {e}", root.display())))?;
        for file in &skill.files {
            let relative = safe_relative(&file.path).ok_or_else(|| {
                BackendError(format!(
                    "skill {}: unsafe file path {}",
                    skill.name, file.path
                ))
            })?;
            let target = root.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    BackendError(format!("could not create {}: {e}", parent.display()))
                })?;
            }
            std::fs::write(&target, &file.bytes)
                .map_err(|e| BackendError(format!("could not write {}: {e}", target.display())))?;
        }
    }
    Ok(())
}

/// Write `value` as pretty JSON, replacing any existing file.
pub fn write_json(path: &Path, value: &Value) -> Result<(), BackendError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| BackendError(format!("could not create {}: {e}", parent.display())))?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| BackendError(format!("could not encode {}: {e}", path.display())))?;
    std::fs::write(path, text)
        .map_err(|e| BackendError(format!("could not write {}: {e}", path.display())))
}

/// The file name the generic MCP JSON is written under.
pub const GENERIC_MCP_FILE: &str = "ralphus-mcp.json";

/// Provision according to `set.delivery`: [`DeliveryMode::BackendDefault`]
/// runs `backend_default` against the backend's isolated directory; extra-args
/// mode writes the generic MCP JSON and skills folder there and expands the
/// configured arguments; [`DeliveryMode::None`] delivers nothing.
pub fn provision_with<F>(
    set: &CapabilitySet,
    isolated_dir: &Path,
    backend_default: F,
) -> Result<Provisioning, BackendError>
where
    F: FnOnce(&Path) -> Result<Provisioning, BackendError>,
{
    match &set.delivery {
        DeliveryMode::None => Ok(Provisioning::default()),
        DeliveryMode::BackendDefault => backend_default(isolated_dir),
        DeliveryMode::ExtraArgs(args) => {
            refuse_tool_filters(set, "extra-args delivery")?;
            let mcp_path = isolated_dir.join(GENERIC_MCP_FILE);
            let skills_dir = isolated_dir.join("skills");
            write_json(&mcp_path, &set.mcp_json())?;
            write_skills(&skills_dir, &set.skills)?;
            let expanded = args
                .iter()
                .map(|a| {
                    a.replace("{mcp_config}", &mcp_path.display().to_string())
                        .replace("{skills_dir}", &skills_dir.display().to_string())
                })
                .collect();
            Ok(Provisioning {
                args: expanded,
                temporary_files: vec![mcp_path],
            })
        }
    }
}

/// Delete the files a provisioning marked temporary. Best-effort.
pub fn cleanup(provisioning: &Provisioning) {
    for path in &provisioning.temporary_files {
        let _ = std::fs::remove_file(path);
    }
}

/// Deletes a [`Provisioning`]'s temporary files when dropped.
pub struct CleanupGuard<'a>(&'a Provisioning);

impl<'a> CleanupGuard<'a> {
    pub fn new(provisioning: &'a Provisioning) -> Self {
        Self(provisioning)
    }
}

impl Drop for CleanupGuard<'_> {
    fn drop(&mut self) {
        cleanup(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ModelBackend;

    fn sample() -> CapabilitySet {
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_string(), "s3cret".to_string());
        CapabilitySet {
            mcp_servers: vec![
                McpServerSpec {
                    name: "files".to_string(),
                    command: Some("npx".to_string()),
                    args: vec!["-y".to_string(), "srv".to_string()],
                    env,
                    ..McpServerSpec::default()
                },
                McpServerSpec {
                    name: "remote".to_string(),
                    url: Some("https://example.test/mcp".to_string()),
                    ..McpServerSpec::default()
                },
            ],
            skills: vec![SkillSpec {
                name: "deploy".to_string(),
                files: vec![
                    SkillFileSpec {
                        path: "SKILL.md".to_string(),
                        bytes: b"# deploy".to_vec(),
                    },
                    SkillFileSpec {
                        path: "scripts/run.sh".to_string(),
                        bytes: b"echo hi".to_vec(),
                    },
                ],
            }],
            delivery: DeliveryMode::BackendDefault,
        }
    }

    #[test]
    fn mcp_json_has_stdio_and_http_entries() {
        let v = sample().mcp_json();
        assert_eq!(v["mcpServers"]["files"]["command"], "npx");
        assert_eq!(v["mcpServers"]["files"]["env"]["TOKEN"], "s3cret");
        assert_eq!(v["mcpServers"]["remote"]["type"], "http");
        assert_eq!(v["mcpServers"]["remote"]["url"], "https://example.test/mcp");
    }

    #[test]
    fn codex_table_uses_command_args_env_and_url() {
        let t = sample().codex_mcp_table();
        let files = t["files"].as_table().unwrap();
        assert_eq!(files["command"].as_str(), Some("npx"));
        assert_eq!(files["args"].as_array().unwrap().len(), 2);
        assert_eq!(files["env"]["TOKEN"].as_str(), Some("s3cret"));
        assert_eq!(
            t["remote"]["url"].as_str(),
            Some("https://example.test/mcp")
        );
    }

    #[test]
    fn from_value_round_trips_wire_shape() {
        let wire = json!({
            "mcp_servers": [{"name": "a", "command": "x", "args": ["1"], "env": {"K": "v"}}],
            "skills": [{"name": "s", "files": [{"path": "SKILL.md", "content_base64": "aGk="}]}],
            "delivery": {"mode": "extra_args", "args": ["--mcp", "{mcp_config}"]},
        });
        let set = CapabilitySet::from_value(&wire).unwrap();
        assert_eq!(set.mcp_servers[0].env["K"], "v");
        assert_eq!(set.skills[0].files[0].bytes, b"hi");
        assert_eq!(
            set.delivery,
            DeliveryMode::ExtraArgs(vec!["--mcp".to_string(), "{mcp_config}".to_string()])
        );
    }

    #[test]
    fn from_value_rejects_a_bad_delivery_mode() {
        assert!(CapabilitySet::from_value(&json!({"delivery": {"mode": "bogus"}})).is_err());
    }

    #[test]
    fn write_skills_replaces_the_directory_and_rejects_escapes() {
        let dir = std::env::temp_dir().join(format!("ralphus-skills-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("stale")).unwrap();
        write_skills(&dir, &sample().skills).unwrap();
        assert!(dir.join("deploy/SKILL.md").is_file());
        assert!(dir.join("deploy/scripts/run.sh").is_file());
        assert!(!dir.join("stale").exists());

        let bad = vec![SkillSpec {
            name: "x".to_string(),
            files: vec![SkillFileSpec {
                path: "../escape".to_string(),
                bytes: Vec::new(),
            }],
        }];
        assert!(write_skills(&dir, &bad).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extra_args_mode_expands_placeholders_and_writes_generic_files() {
        let dir = std::env::temp_dir().join(format!("ralphus-extra-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut set = sample();
        set.delivery =
            DeliveryMode::ExtraArgs(vec!["--mcp-config".to_string(), "{mcp_config}".to_string()]);
        let p = provision_with(&set, &dir, |_| panic!("backend default must not run")).unwrap();
        assert_eq!(p.args[0], "--mcp-config");
        assert!(p.args[1].ends_with(GENERIC_MCP_FILE));
        assert!(dir.join(GENERIC_MCP_FILE).is_file());
        cleanup(&p);
        assert!(!dir.join(GENERIC_MCP_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn none_mode_delivers_nothing() {
        let mut set = sample();
        set.delivery = DeliveryMode::None;
        let p = provision_with(&set, Path::new("/nonexistent"), |_| panic!("unused")).unwrap();
        assert_eq!(p, Provisioning::default());
    }

    fn unique_workspace(label: &str) -> crate::tools::Workspace {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-cap-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::tools::Workspace::create(dir).unwrap()
    }

    #[test]
    fn claude_provisioning_passes_strict_mcp_config_and_copies_skills() {
        let ws = unique_workspace("claude");
        let backend = crate::claude_code_backend::ClaudeCodeBackend {
            keep_temporary_files: false,
            program_override: None,
        };
        let set = sample();
        let options = crate::backend::RunOptions {
            allow_personal_settings: true,
            allow_personal_memory: true,
            capabilities: Some(&set),
            ..Default::default()
        };
        let p = backend.provision_capabilities(&ws, &options, &set).unwrap();
        assert!(p.args.contains(&"--strict-mcp-config".to_string()));
        let at = p.args.iter().position(|a| a == "--mcp-config").unwrap();
        let json: Value =
            serde_json::from_str(&std::fs::read_to_string(&p.args[at + 1]).unwrap()).unwrap();
        assert_eq!(json["mcpServers"]["files"]["env"]["TOKEN"], "s3cret");
        let dir = crate::agent_isolation::isolated_config_dir(ws.root(), "claude-code");
        assert!(dir.join("skills/deploy/SKILL.md").exists());
        cleanup(&p);
    }

    #[test]
    fn codex_provisioning_replaces_personal_mcp_servers_even_with_settings_allowed() {
        let ws = unique_workspace("codex");
        let dir = crate::agent_isolation::isolated_config_dir(ws.root(), "codex");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "model = \"gpt\"
[mcp_servers.personal]
command = \"x\"
",
        )
        .unwrap();
        let backend = crate::codex_backend::CodexBackend {
            keep_temporary_files: false,
            program_override: None,
        };
        let set = sample();
        let options = crate::backend::RunOptions {
            allow_personal_settings: true,
            capabilities: Some(&set),
            ..Default::default()
        };
        backend.provision_capabilities(&ws, &options, &set).unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        let table: toml::Table = text.parse().unwrap();
        assert_eq!(table["model"].as_str(), Some("gpt"));
        let servers = table["mcp_servers"].as_table().unwrap();
        assert!(servers.contains_key("files") && servers.contains_key("remote"));
        assert!(!servers.contains_key("personal"));
        assert!(dir.join("skills/deploy/scripts/run.sh").exists());
    }

    #[test]
    fn pi_provisioning_writes_mcp_json_adapter_setting_and_skills() {
        let ws = unique_workspace("pi");
        let backend = crate::pi_backend::PiBackend {
            keep_temporary_files: false,
            program_override: None,
        };
        let set = sample();
        let options = crate::backend::RunOptions {
            capabilities: Some(&set),
            ..Default::default()
        };
        backend.provision_capabilities(&ws, &options, &set).unwrap();
        let dir = crate::agent_isolation::isolated_config_dir(ws.root(), "pi");
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
        assert!(mcp["mcpServers"]["files"].is_object());
        let settings = std::fs::read_to_string(dir.join("settings.json")).unwrap();
        assert!(settings.contains("pi-mcp-adapter"));
        assert!(dir.join("skills/deploy/SKILL.md").exists());
    }

    fn with_denied(tools: &[&str]) -> CapabilitySet {
        let mut set = sample();
        set.mcp_servers[0].denied_tools = tools.iter().map(|t| (*t).to_string()).collect();
        set
    }

    #[test]
    fn claude_provisioning_passes_disallowed_tools_before_the_mcp_flags() {
        let ws = unique_workspace("claude-deny");
        let backend = crate::claude_code_backend::ClaudeCodeBackend {
            keep_temporary_files: false,
            program_override: None,
        };
        let set = with_denied(&["run_js", "page.eval"]);
        let options = crate::backend::RunOptions {
            capabilities: Some(&set),
            ..Default::default()
        };
        let p = backend.provision_capabilities(&ws, &options, &set).unwrap();
        let at = p
            .args
            .iter()
            .position(|a| a == "--disallowedTools")
            .unwrap();
        assert_eq!(
            p.args[at + 1],
            format!(
                "mcp__{0}__run_js,mcp__{0}__page_eval",
                set.mcp_servers[0].name
            )
        );
        assert!(at + 2 < p.args.len() && p.args[at + 2] == "--mcp-config");
        assert!(with_denied(&[]).claude_disallowed_tools().is_none());
    }

    #[test]
    fn codex_table_lists_disabled_tools() {
        let set = with_denied(&["run_js"]);
        let table = set.codex_mcp_table();
        let entry = table[&set.mcp_servers[0].name].as_table().unwrap();
        assert_eq!(entry["disabled_tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn pi_and_extra_args_refuse_denied_tools() {
        let ws = unique_workspace("pi-deny");
        let backend = crate::pi_backend::PiBackend {
            keep_temporary_files: false,
            program_override: None,
        };
        let set = with_denied(&["run_js"]);
        let options = crate::backend::RunOptions::default();
        let err = backend
            .provision_capabilities(&ws, &options, &set)
            .unwrap_err();
        assert!(err.0.contains("cannot filter"));
        let mut generic = with_denied(&["run_js"]);
        generic.delivery = DeliveryMode::ExtraArgs(vec!["--x".to_string()]);
        let dir = std::env::temp_dir().join("ralphus-cap-deny-generic");
        assert!(provision_with(&generic, &dir, |_| Ok(Provisioning::default())).is_err());
    }

    #[test]
    fn unsupported_backend_reports_no_support() {
        let backend = crate::harness_backend::HarnessBackend {
            program: "aider".to_string(),
        };
        assert!(!backend.supports_capabilities());
    }
}
