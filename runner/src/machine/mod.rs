//! The Machine API verbs, served on the machine itself.
//!
//! `ralphus-ssh-provider` drives a Linux machine with POSIX shell scripts. A
//! machine where that does not work -- Windows over SSH, or a machine reached
//! through `ralphus-outpost` -- runs the verbs here instead, natively, with no
//! shell in between: [`execute`] takes a verb's JSON request (the same
//! request the v1 verb of that name reads on stdin) and returns its reply
//! fields plus any events. Two hosts call it:
//!
//! - `ralphus-runner machine <verb> --remote-root <root>` ([`cli`]), which
//!   `ralphus-ssh-provider` runs over SSH on a Windows machine with the
//!   request on the session's stdin;
//! - `ralphus-outpost`, which receives requests from `ralphus-fleet` over its
//!   outbound connection and calls [`execute`] in-process.
//!
//! Everything a request names is confined to the machine's `remote_root`
//! ([`paths`]); projects are laid out exactly as the SSH provider lays them
//! out ([`ralphus_core::machine_layout`]). Credentials (`execution_environment`
//! of an `exec`, `env` of a `provision` or `run`) only ever reach a child
//! process's environment -- never a file, an argument or an event.

pub mod cli;
mod command;
mod files;
mod jobs;
mod lock;
pub mod paths;
mod provision;
mod spawn;

use serde_json::{Value, json};

/// The verbs [`execute`] serves.
pub const SUPPORTED_OPS: &[&str] = &[
    "provision",
    "exec",
    "status",
    "stream",
    "cancel",
    "job-cleanup",
    "ping",
    "run",
    "read-file",
    "write-file",
    "remove-path",
    "materialize",
    "cleanup",
    "capabilities",
    "preflight",
];

/// The provider contract version replies carry (the v1 verb shape).
pub const PROTOCOL_VERSION: i64 = 1;

/// Where [`execute`] runs.
#[derive(Debug, Clone)]
pub struct Context {
    /// The machine's storage root, absolute in the machine's own form.
    /// Everything a request touches must be under it.
    pub remote_root: String,
    /// The program and leading arguments that reach this runner's command
    /// line: `[ralphus-runner]`, or `[ralphus-outpost, runner]`. Jobs run
    /// `<runner...> machine supervise` and `<runner...> send` through it.
    pub runner: Vec<String>,
}

impl Context {
    /// A context whose runner is this executable.
    ///
    /// # Errors
    /// The executable's path cannot be found.
    pub fn for_current_exe(remote_root: &str, leading_args: &[&str]) -> Result<Self, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("could not find this executable's path: {e}"))?;
        let mut runner = vec![exe.to_string_lossy().into_owned()];
        runner.extend(leading_args.iter().map(|a| (*a).to_string()));
        Ok(Self {
            remote_root: remote_root.to_string(),
            runner,
        })
    }
}

/// What one verb produced: the reply's fields and the events to forward.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    /// `{"ok": ..., <verb fields>}` with `protocol_version`.
    pub body: Value,
    /// Event JSON objects (as text), oldest first, in the shape
    /// `RALPHUS_EVENT:` lines carry.
    pub events: Vec<String>,
}

/// Events recorded while a verb runs.
#[derive(Debug, Default)]
pub(crate) struct Events(Vec<String>);

impl Events {
    /// Record one event. Never put a credential, a spec body or an
    /// environment value in `payload`.
    pub(crate) fn emit(&mut self, message: &str, level: &str, payload: Value) {
        self.0.push(
            json!({
                "source": "machine",
                "message": message,
                "level": level,
                "payload": payload,
            })
            .to_string(),
        );
    }

    /// Record an event line the runner already wrote (`RALPHUS_EVENT: {...}`).
    pub(crate) fn forward(&mut self, line: &str) {
        if let Some(event) = line.trim().strip_prefix(EVENT_PREFIX) {
            let event = event.trim();
            if !event.is_empty() {
                self.0.push(event.to_string());
            }
        }
    }
}

/// The marker event lines start with.
pub const EVENT_PREFIX: &str = "RALPHUS_EVENT:";

fn ok(fields: Value) -> Value {
    let mut body = json!({"ok": true, "protocol_version": PROTOCOL_VERSION});
    if let (Some(body), Value::Object(fields)) = (body.as_object_mut(), fields) {
        body.extend(fields);
    }
    body
}

fn err(message: &str) -> Value {
    json!({"ok": false, "protocol_version": PROTOCOL_VERSION, "error": message})
}

/// Serve `verb` with `request` on this machine.
#[must_use]
pub fn execute(verb: &str, request: &Value, context: &Context) -> Reply {
    let mut events = Events::default();
    let result = run_verb(verb, request, context, &mut events);
    let body = match result {
        Ok(fields) => ok(fields),
        Err(message) => {
            if verb_reports_failures(verb) {
                events.emit(
                    "verb failed",
                    "warning",
                    json!({"verb": verb, "error": ralphus_core::redact::redact_secrets(&message)}),
                );
            }
            err(&message)
        }
    };
    Reply {
        body,
        events: events.0,
    }
}

/// Polling verbs run every few seconds; their failures are in the reply and
/// would only bury the timeline as events.
fn verb_reports_failures(verb: &str) -> bool {
    !matches!(verb, "status" | "stream" | "read-file" | "capabilities")
}

fn run_verb(
    verb: &str,
    request: &Value,
    context: &Context,
    events: &mut Events,
) -> Result<Value, String> {
    let root = paths::Root::new(&context.remote_root)?;
    match verb {
        "capabilities" => Ok(json!({"capabilities": capabilities()})),
        "ping" => Ok(json!({"detail": ping_detail()})),
        "provision" => {
            provision::run(request, &root, events).map(|workspace| json!({"workspace": workspace}))
        }
        "run" => command::run(request, &root)
            .map(|(stdout, exit_code)| json!({"stdout": stdout, "exit_code": exit_code})),
        "read-file" => files::read_file(request, &root).map(|content| json!({"stdout": content})),
        "write-file" => files::write_file(request, &root).map(|()| json!({})),
        "remove-path" => files::remove_path(request, &root, events).map(|()| json!({})),
        "materialize" => files::materialize(request, &root).map(|files| json!({"files": files})),
        "cleanup" => files::cleanup(request, &root).map(|removed| json!({"removed": removed})),
        "exec" => jobs::exec(request, &root, context, events),
        "status" => jobs::status(request, &root),
        "stream" => jobs::stream(request, &root, events),
        "cancel" => jobs::cancel(request, &root, events).map(|()| json!({})),
        "job-cleanup" => jobs::cleanup(request, &root).map(|()| json!({})),
        "preflight" => preflight(request),
        "retire" => Err(
            "this machine does not implement \"retire\"; see docs/machine-providers.md for its \
             supported verbs"
                .to_string(),
        ),
        other => Err(format!("unknown verb {other:?}")),
    }
}

/// `preflight`: whether the agent backend `agent` (launched through
/// `executable` when given) can be started on this machine, checked exactly
/// as `ralphus-runner preflight` checks it. An unavailable agent is a normal
/// answer (`available: false` with the reason in `detail`), not a refusal.
fn preflight(request: &Value) -> Result<Value, String> {
    let agent = required_str(request, "agent", "a preflight request")?;
    let executable = request
        .get("executable")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    Ok(
        match crate::execute::preflight_agent(agent, executable, false) {
            Ok(()) => json!({"available": true}),
            Err(detail) => json!({"available": false, "detail": detail}),
        },
    )
}

/// `capabilities`: what this machine is and serves.
fn capabilities() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "supported_ops": SUPPORTED_OPS,
        "async_exec": true,
        "runner_version": format!("ralphus-runner {}", ralphus_core::version()),
    })
}

fn ping_detail() -> String {
    let host = gethostname_lossy();
    format!(
        "reachable: {host} ({} {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

fn gethostname_lossy() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "this machine".to_string())
}

/// A required, non-empty string field of `value`.
fn required_str<'a>(value: &'a Value, field: &str, what: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{what} is missing a non-empty {field:?}"))
}

/// Whether `key` is a valid environment variable name.
fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
        && !key.starts_with(|c: char| c.is_ascii_digit())
}

/// Parse an optional `{name: value}` map of environment variables.
fn env_map(
    value: Option<&Value>,
    what: &str,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(std::collections::BTreeMap::new());
    };
    let map: std::collections::BTreeMap<String, String> = serde_json::from_value(value.clone())
        .map_err(|e| format!("{what} must map names to string values: {e}"))?;
    if let Some(bad) = map.keys().find(|k| !valid_env_key(k)) {
        return Err(format!(
            "invalid environment variable name in {what}: {bad:?}"
        ));
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn context(root: &std::path::Path) -> Context {
        Context {
            remote_root: root.to_string_lossy().replace('\\', "/"),
            runner: vec!["ralphus-runner".to_string()],
        }
    }

    #[test]
    fn capabilities_report_this_machine_and_every_verb() {
        let dir = ralphus_test_support::TestDir::new("machine-capabilities");
        let reply = execute("capabilities", &json!({}), &context(&dir));
        assert_eq!(reply.body["ok"], true, "{}", reply.body);
        let caps = &reply.body["capabilities"];
        assert_eq!(caps["os"], std::env::consts::OS);
        assert_eq!(caps["async_exec"], true);
        for op in SUPPORTED_OPS {
            assert!(
                caps["supported_ops"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v == op),
                "{op}"
            );
        }
    }

    #[test]
    fn an_unknown_verb_and_retire_are_refused() {
        let dir = ralphus_test_support::TestDir::new("machine-unknown-verb");
        let reply = execute("format-disk", &json!({}), &context(&dir));
        assert_eq!(reply.body["ok"], false);
        let reply = execute("retire", &json!({}), &context(&dir));
        assert!(
            reply.body["error"]
                .as_str()
                .unwrap()
                .contains("does not implement"),
            "{}",
            reply.body
        );
    }

    #[test]
    fn preflight_answers_whether_the_agent_can_start_here() {
        let dir = ralphus_test_support::TestDir::new("machine-preflight");
        let missing = execute(
            "preflight",
            &json!({"agent": "codex", "executable": null}),
            &context(&dir),
        );
        // `codex` resolves on some hosts, so only the reply shape is fixed:
        // an answer, never a refusal.
        assert_eq!(missing.body["ok"], true, "{}", missing.body);
        assert!(missing.body["available"].is_boolean(), "{}", missing.body);
        let unknown = execute("preflight", &json!({"agent": "no-such"}), &context(&dir));
        assert_eq!(unknown.body["ok"], true, "{}", unknown.body);
        assert_eq!(unknown.body["available"], false);
        assert!(
            unknown.body["detail"]
                .as_str()
                .unwrap()
                .contains("unknown backend"),
            "{}",
            unknown.body
        );
        let no_agent = execute("preflight", &json!({}), &context(&dir));
        assert_eq!(no_agent.body["ok"], false);
    }

    #[test]
    fn a_relative_remote_root_is_refused() {
        let reply = execute(
            "ping",
            &json!({}),
            &Context {
                remote_root: "relative/root".to_string(),
                runner: vec![],
            },
        );
        assert_eq!(reply.body["ok"], false);
    }

    #[test]
    fn environment_maps_are_validated() {
        assert!(env_map(Some(&json!({"A_1": "x"})), "env").is_ok());
        assert!(env_map(Some(&json!({"1A": "x"})), "env").is_err());
        assert!(env_map(Some(&json!({"A-B": "x"})), "env").is_err());
        assert!(env_map(Some(&json!({"A": 1})), "env").is_err());
        assert!(env_map(None, "env").unwrap().is_empty());
    }
}
