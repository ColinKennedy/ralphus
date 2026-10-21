//! An allocation's Machine API, served by running the matching verb of the
//! SSH machine provider (contract v1) against the allocated machine: the
//! request body goes to its stdin, its stdout is the reply, and the runner's
//! `RALPHUS_EVENT:` lines on its stderr come back as the reply's `events`.
//!
//! The service fills in what it owns about a machine -- its storage root,
//! runner policy and agent paths -- so ralphus never needs to know them.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::config::{FleetConfig, MachineConfig};

/// Marker the runner prefixes its events with on stderr.
const EVENT_MARKER: &str = "RALPHUS_EVENT:";

/// The contract version every reply carries.
pub const PROTOCOL_VERSION: i64 = 2;

/// Verbs a Machine API serves.
pub const VERBS: [&str; 14] = [
    "provision",
    "exec",
    "status",
    "stream",
    "cancel",
    "run",
    "read-file",
    "write-file",
    "remove-path",
    "materialize",
    "capabilities",
    "ping",
    "cleanup",
    "retire",
];

/// A refusal in the contract's error shape.
#[must_use]
pub fn refusal(reason_code: &str, error: &str, remediation: &str) -> Value {
    json!({
        "ok": false,
        "protocol_version": PROTOCOL_VERSION,
        "error": error,
        "reason_code": reason_code,
        "remediation": remediation,
    })
}

/// The SSH provider program to run.
#[must_use]
pub fn ssh_provider_program(config: &FleetConfig) -> PathBuf {
    if let Some(path) = &config.ssh_provider {
        return PathBuf::from(path);
    }
    let name = format!("ralphus-ssh-provider{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Add what the service owns about `machine` to a verb's request.
fn complete_request(verb: &str, machine: &MachineConfig, body: &mut Value) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    let missing = |fields: &serde_json::Map<String, Value>, key: &str| {
        fields.get(key).is_none_or(Value::is_null)
    };
    if matches!(verb, "provision" | "cleanup") && missing(fields, "remote_root") {
        fields.insert("remote_root".into(), machine.remote_root.clone().into());
    }
    if verb == "provision" && missing(fields, "runner") {
        fields.insert("runner".into(), runner_policy(machine));
    }
    if verb == "exec" {
        let agent = fields
            .get("agent")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(path) = agent.and_then(|a| machine.agents.get(&a).cloned()) {
            fields.insert("executable".into(), path.into());
        }
    }
}

/// The SSH provider's target runner policy for `machine`.
fn runner_policy(machine: &MachineConfig) -> Value {
    json!({
        "mode": machine.runner.mode,
        "command": machine.runner.command,
        "artifacts": machine.runner.artifacts,
        "remote_root": machine.remote_root,
    })
}

/// What one provider invocation produced.
struct Invocation {
    stdout: String,
    events: Vec<String>,
    stderr_tail: String,
    status: Option<i32>,
}

/// Run `verb` of the SSH provider against `machine` with `body` on stdin.
fn invoke(
    config: &FleetConfig,
    machine: &MachineConfig,
    verb: &str,
    body: &Value,
    extra: &[String],
) -> Result<Invocation, String> {
    let program = ssh_provider_program(config);
    let mut command = Command::new(&program);
    command
        .arg(verb)
        .arg("--uri")
        .arg(machine.ssh_uri())
        .args(extra)
        .env(
            "RALPHUS_TARGET_RUNNER_CONFIG",
            runner_policy(machine).to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(file) = &config.ssh_config_file {
        command.env("RALPHUS_SSH_CONFIG_FILE", file);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", program.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body.to_string().as_bytes());
    }
    let stderr = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut events = Vec::new();
            let mut tail = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                match line.trim_end().strip_prefix(EVENT_MARKER) {
                    Some(event) => events.push(event.to_string()),
                    None if tail.len() < 4096 => {
                        tail.push_str(&line);
                        tail.push('\n');
                    }
                    None => {}
                }
            }
            (events, tail)
        })
    });
    let output = child
        .wait_with_output()
        .map_err(|e| format!("could not wait for {}: {e}", program.display()))?;
    let (events, stderr_tail) = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    Ok(Invocation {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        events,
        stderr_tail,
        status: output.status.code(),
    })
}

/// Serve `verb` of the Machine API for `machine`: the reply body.
#[must_use]
pub fn serve(config: &FleetConfig, machine: &MachineConfig, verb: &str, mut body: Value) -> Value {
    if !VERBS.contains(&verb) {
        return refusal(
            "unsupported",
            &format!("no Machine API operation {verb:?}"),
            "use one of the operations in docs/machine-provider-service.md",
        );
    }
    if !body.is_object() {
        body = json!({});
    }
    // v1 takes the job handle and cursor as flags, not stdin.
    let mut extra = Vec::new();
    if let Some(fields) = body.as_object_mut() {
        if let Some(handle) = fields
            .remove("handle")
            .and_then(|h| h.as_str().map(str::to_string))
        {
            extra.extend(["--handle".to_string(), handle]);
        }
        if let Some(since) = fields.remove("since").and_then(|s| s.as_i64()) {
            extra.extend(["--since".to_string(), since.to_string()]);
        }
    }
    complete_request(verb, machine, &mut body);
    if verb == "materialize" {
        return materialize(config, machine, body);
    }
    reply_from(config, machine, verb, &body, &extra)
}

fn reply_from(
    config: &FleetConfig,
    machine: &MachineConfig,
    verb: &str,
    body: &Value,
    extra: &[String],
) -> Value {
    let invocation = match invoke(config, machine, verb, body, extra) {
        Ok(invocation) => invocation,
        Err(e) => {
            return refusal(
                "unreachable",
                &e,
                "install ralphus-ssh-provider next to ralphus-fleet or set ssh_provider in its config",
            );
        }
    };
    let mut reply: Value = match serde_json::from_str(invocation.stdout.trim()) {
        Ok(reply) => reply,
        Err(_) => {
            return refusal(
                "unreachable",
                &format!(
                    "machine {:?} gave no reply to {verb} (exit {}): {}",
                    machine.display_name(),
                    invocation.status.unwrap_or(-1),
                    invocation.stderr_tail.trim()
                ),
                "check that the machine is reachable over SSH from the fleet host",
            );
        }
    };
    if let Some(fields) = reply.as_object_mut() {
        fields.insert("protocol_version".into(), PROTOCOL_VERSION.into());
        fields.insert("events".into(), invocation.events.into());
    }
    reply
}

/// `materialize`: let the provider copy the selected path onto this host,
/// then return its files in the reply, since ralphus's own host may be
/// another machine.
fn materialize(config: &FleetConfig, machine: &MachineConfig, mut body: Value) -> Value {
    let staging = match staging_dir() {
        Ok(dir) => dir,
        Err(e) => return refusal("unreachable", &e, "check the fleet host's temp directory"),
    };
    let destination = staging.join("out");
    let executable = body["executable"].as_bool().unwrap_or(false);
    body["destination"] = destination.to_string_lossy().into_owned().into();
    let mut reply = reply_from(config, machine, "materialize", &body, &[]);
    if reply["ok"] == true {
        match collect_files(&destination, executable) {
            Ok(files) => reply["files"] = files.into(),
            Err(e) => {
                reply = refusal(
                    "unreachable",
                    &format!("could not read back the materialized files: {e}"),
                    "check the fleet host's temp directory",
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
    reply
}

fn staging_dir() -> Result<PathBuf, String> {
    let mut nonce = [0u8; 8];
    getrandom::getrandom(&mut nonce).map_err(|e| format!("no randomness: {e}"))?;
    let name: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().join(format!("ralphus-fleet-{name}"));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// The files under `root` (or `root` itself, when it is a file) as
/// `{path, content_base64, executable}` entries with `/`-separated paths.
fn collect_files(root: &Path, executable: bool) -> Result<Vec<Value>, String> {
    use base64::Engine as _;
    let encode = |path: &Path| {
        std::fs::read(path)
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
            .map_err(|e| format!("{}: {e}", path.display()))
    };
    if root.is_file() {
        return Ok(vec![json!({
            "path": "",
            "content_base64": encode(root)?,
            "executable": executable,
        })]);
    }
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push(json!({
                "path": relative,
                "content_base64": encode(&path)?,
                "executable": false,
            }));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> MachineConfig {
        FleetConfig::parse(
            "token_env = \"T\"\n[[machine]]\nhost = \"box\"\nremote_root = \"/srv/fleet\"\n\
             [machine.agents]\ncodex = \"/opt/codex\"\n",
            "f",
        )
        .unwrap()
        .machines
        .remove(0)
    }

    #[test]
    fn the_service_fills_in_what_it_owns() {
        let m = machine();
        let mut provision = json!({"project": "p", "remote_root": null});
        complete_request("provision", &m, &mut provision);
        assert_eq!(provision["remote_root"], "/srv/fleet");
        assert_eq!(provision["runner"]["remote_root"], "/srv/fleet");

        let mut exec = json!({"agent": "codex", "executable": null});
        complete_request("exec", &m, &mut exec);
        assert_eq!(exec["executable"], "/opt/codex");

        let mut other = json!({"agent": "claude"});
        complete_request("exec", &m, &mut other);
        assert!(other.get("executable").is_none());
    }

    #[test]
    fn an_unknown_operation_is_refused() {
        let config = FleetConfig::parse("token_env = \"T\"\n", "f").unwrap();
        let reply = serve(&config, &machine(), "format-disk", json!({}));
        assert_eq!(reply["reason_code"], "unsupported");
    }

    #[test]
    fn a_materialized_directory_comes_back_as_relative_files() {
        let root = std::env::temp_dir().join(format!("fleet-collect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("clips")).unwrap();
        std::fs::write(root.join("a.png"), "a").unwrap();
        std::fs::write(root.join("clips").join("b.gif"), "b").unwrap();
        let mut paths: Vec<String> = collect_files(&root, false)
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, ["a.png", "clips/b.gif"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
