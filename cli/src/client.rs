//! `DaemonClient`, ported from `cli/src/ralphus/client.py`. A thin HTTP/JSON
//! client over the daemon's API (`docs/daemon-api.md`) -- every method builds
//! a payload/query string and calls one of the three private HTTP verbs,
//! which fold errors into [`DaemonError`] and (RAL-203) redact guardian
//! env-var values before returning a successful response.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7890";
const DEFAULT_DAEMON_TIMEOUT_SECS: u64 = 60;

/// A daemon call failed. `status_code` is `None` when the daemon could not be
/// reached at all (connection refused/timeout) -- distinct from a reached
/// daemon rejecting the request (`Some(404)`, `Some(409)`, ...). Mirrors
/// Python's `DaemonError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonError {
    pub message: String,
    pub status_code: Option<u16>,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DaemonError {}

pub struct DaemonClient {
    base_url: String,
    timeout: Duration,
}

impl DaemonClient {
    /// `timeout` overrides `$RALPHUS_DAEMON_TIMEOUT` (seconds), which
    /// overrides the default of 60s -- deliberately generous, since a
    /// mutating request can be serialized behind the daemon's own
    /// single-threaded HTTP loop and a short timeout would misreport
    /// "unreachable" for something merely slow.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        let secs = std::env::var("RALPHUS_DAEMON_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_DAEMON_TIMEOUT_SECS);
        Self {
            base_url: base_url.into(),
            timeout: Duration::from_secs(secs),
        }
    }

    #[must_use]
    pub fn with_timeout(base_url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            base_url: base_url.into(),
            timeout,
        }
    }

    #[must_use]
    pub fn default_url() -> &'static str {
        DEFAULT_DAEMON_URL
    }

    /// The daemon URL this client was constructed with -- `ralphus-mcp`
    /// needs it back out for `health::run_checks`, which takes a raw URL
    /// string rather than a `DaemonClient` (RAL-301).
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// Attach `Authorization: Bearer <token>` when a token is available, so
    /// the daemon's RAL-219 auth gate doesn't reject every request. Mirrors
    /// `librarian/src/server.rs::daemon_token()`/`proxy()`.
    fn authorize(&self, req: ureq::Request) -> ureq::Request {
        match daemon_token() {
            Some(token) => req.set("Authorization", &format!("Bearer {token}")),
            None => req,
        }
    }

    fn get(&self, path: &str) -> Result<Value, DaemonError> {
        let req = self.authorize(ureq::get(&self.url(path)).timeout(self.timeout));
        self.finish(path, req.call())
    }

    fn delete(&self, path: &str) -> Result<Value, DaemonError> {
        let req = self.authorize(ureq::delete(&self.url(path)).timeout(self.timeout));
        self.finish(path, req.call())
    }

    fn post(&self, path: &str, payload: Option<Value>) -> Result<Value, DaemonError> {
        let req = self.authorize(
            ureq::post(&self.url(path))
                .timeout(self.timeout)
                .set("content-type", "application/json"),
        );
        let body = payload.unwrap_or_else(|| json!({}));
        self.finish(path, req.send_string(&body.to_string()))
    }

    fn finish(
        &self,
        path: &str,
        result: Result<ureq::Response, ureq::Error>,
    ) -> Result<Value, DaemonError> {
        let resp = result.map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                let body: Value = resp
                    .into_string()
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or(Value::Null);
                DaemonError {
                    message: extract_error_message(&body).unwrap_or_else(|| format!("HTTP {code}")),
                    status_code: Some(code),
                }
            }
            other => DaemonError {
                message: format!("could not reach daemon: {other}"),
                status_code: None,
            },
        })?;
        let text = resp.into_string().map_err(|e| DaemonError {
            message: format!("could not reach daemon: {e}"),
            status_code: None,
        })?;
        let mut value: Value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        if path.starts_with("/api/guardians") {
            redact_guardian_env_values(&mut value);
        }
        Ok(value)
    }
}

/// Read the daemon's bearer token (RAL-219). `$RALPHUS_DAEMON_TOKEN`, when
/// set, wins over the token file -- useful for a remote/non-local daemon
/// where the CLI has no filesystem access to `state_dir()/daemon.token`.
/// Otherwise reads the same file the daemon itself generates/persists at
/// startup, mirroring `librarian/src/server.rs::daemon_token()`.
fn daemon_token() -> Option<String> {
    if let Ok(token) = std::env::var("RALPHUS_DAEMON_TOKEN") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    std::fs::read_to_string(ralphus_core::daemon_token_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn extract_error_message(body: &Value) -> Option<String> {
    body["error"]["message"].as_str().map(str::to_string)
}

/// RAL-203: replaces every *value* (never a key, and never a `null`
/// tombstone in a `*_env_overrides`/`env_overrides` map) under any
/// `combined_env`/`build_env`/`manual_checks_env`/`resolved_env`/
/// `inherited_env`/`env_overrides`/`*_env_overrides` key, anywhere in the
/// response, with `"<hidden>"`. A single chokepoint so no call site has to
/// remember to redact secrets before printing.
///
/// RAL-324's resolved-env views ([`DaemonClient::env_view`]) are outside this
/// on purpose: their payload is a list of `{name, value, ...}` *rows*, not a
/// `{key: value}` map under one of the names above, and the daemon has already
/// masked every value whose name the Secrets tab registers. Those views are
/// the one place a guardian environment *value* is printed, and they print
/// exactly what the board's popup shows.
fn redact_guardian_env_values(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_env_map_key(key) {
                    redact_env_map(val, is_tombstone_map_key(key));
                } else {
                    redact_guardian_env_values(val);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_guardian_env_values(item);
            }
        }
        _ => {}
    }
}

fn is_env_map_key(key: &str) -> bool {
    matches!(
        key,
        "combined_env"
            | "build_env"
            | "manual_checks_env"
            | "resolved_env"
            | "inherited_env"
            | "env_overrides"
    ) || key.ends_with("_env_overrides")
}

fn is_tombstone_map_key(key: &str) -> bool {
    key == "env_overrides" || key.ends_with("_env_overrides")
}

fn redact_env_map(value: &mut Value, preserve_null_tombstones: bool) {
    if let Value::Object(map) = value {
        for (_, val) in map.iter_mut() {
            if preserve_null_tombstones && val.is_null() {
                continue;
            }
            *val = Value::String("<hidden>".to_string());
        }
    }
}

/// Builds a query string from `(key, value)` pairs, skipping `None` values.
fn query_string(pairs: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = pairs
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={}", urlencode(v))))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Only-set-if-`Some` insertion, matching Python's `if value is not None:
/// payload[key] = value` idiom used throughout `client.py`.
fn set_if_some<T: Into<Value>>(obj: &mut Value, key: &str, value: Option<T>) {
    if let Some(v) = value {
        obj[key] = v.into();
    }
}

impl DaemonClient {
    // ---- health / validate / submit ------------------------------------

    pub fn health(&self) -> Result<Value, DaemonError> {
        self.get("/api/daemon")
    }

    pub fn validate(&self, toml_text: &str) -> Result<Value, DaemonError> {
        self.post("/api/squads/validate", Some(json!({"toml": toml_text})))
    }

    pub fn submit(
        &self,
        toml_text: &str,
        hold: bool,
        label: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({"toml": toml_text, "hold": hold});
        set_if_some(&mut body, "label", label.map(str::to_string));
        self.post("/api/squads", Some(body))
    }

    // ---- board / squads ----------------------------------------------------

    pub fn tasks(
        &self,
        status: Option<&str>,
        name: Option<&str>,
        sort: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let qs = query_string(&[
            ("status", status.map(str::to_string)),
            ("name", name.map(str::to_string)),
            ("sort", sort.map(str::to_string)),
        ]);
        self.get(&format!("/api/tasks{qs}"))
    }

    pub fn squad(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/squads/{squad_id}"))
    }

    /// One surface's resolved environment (RAL-324). `api_path` is the very
    /// path that surface's overrides are *set* on -- the daemon serves each
    /// read-only view as a `GET` twin of the matching `POST .../env` route --
    /// so callers build the same path string the board's popup fetches, and
    /// receive values already masked against the Secrets tab's registered
    /// names.
    pub fn env_view(&self, api_path: &str) -> Result<Value, DaemonError> {
        self.get(api_path)
    }

    pub fn cancel(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/squads/{squad_id}/cancel"), None)
    }

    pub fn queue(&self) -> Result<Value, DaemonError> {
        self.get("/api/queue")
    }

    pub fn queue_reorder(&self, order: &[String]) -> Result<Value, DaemonError> {
        self.post("/api/queue/reorder", Some(json!({"order": order})))
    }

    pub fn queue_set_position(
        &self,
        items: &[String],
        position: i64,
        absolute: bool,
    ) -> Result<Value, DaemonError> {
        self.post(
            "/api/queue/set-position",
            Some(json!({"items": items, "position": position, "absolute": absolute})),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_status(
        &self,
        squad_id: &str,
        state: &str,
        kind: &str,
        task_idx: i64,
        cell_idx: i64,
        proof_idx: i64,
        proof_scope: &str,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/set-status"),
            Some(json!({
                "state": state, "kind": kind, "task_idx": task_idx,
                "cell_idx": cell_idx, "proof_idx": proof_idx,
                "proof_scope": proof_scope,
            })),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_project(
        &self,
        name: &str,
        path: &str,
        description: &str,
        vcs: &str,
        clone_url: Option<&str>,
        clear_clone_url: bool,
        match_pr_branch_name: Option<bool>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({"name": name, "path": path, "description": description, "vcs": vcs});
        set_if_some(&mut body, "clone_url", clone_url);
        if clear_clone_url {
            body["clear_clone_url"] = json!(true);
        }
        set_if_some(&mut body, "match_pr_branch_name", match_pr_branch_name);
        self.post("/api/projects", Some(body))
    }

    pub fn list_projects(&self) -> Result<Value, DaemonError> {
        self.get("/api/projects")
    }

    pub fn get_project(&self, name: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/projects/{name}"))
    }

    /// Agent-profile health, evaluated inside the daemon process so
    /// `from_env`/`executable` resolution reflects the daemon's own
    /// environment/PATH rather than the CLI's -- see
    /// `ralphus_daemon::agent_profiles::check_profiles_health`.
    pub fn health_agent_profiles(&self, cwd: &Path) -> Result<Value, DaemonError> {
        let qs = query_string(&[("cwd", Some(cwd.to_string_lossy().into_owned()))]);
        self.get(&format!("/api/health/agent-profiles{qs}"))
    }

    /// Agents selectable for `cwd` -- built-in backends plus configured
    /// `.ralphus.toml` agent profiles -- plus the effective
    /// `[review].default_resolver_agent`. Backs the board's review-resolver
    /// dropdown and `ralphus check health`'s default-resolver-agent check.
    pub fn list_agents(&self, cwd: &Path) -> Result<Value, DaemonError> {
        let qs = query_string(&[("cwd", Some(cwd.to_string_lossy().into_owned()))]);
        self.get(&format!("/api/agents{qs}"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_machine(
        &self,
        scheme: &str,
        program: &str,
        description: &str,
        args: Option<&[String]>,
        protocol_version: Option<&str>,
        supports_channel: bool,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({
            "scheme": scheme, "program": program, "description": description,
            "supports_channel": supports_channel,
        });
        set_if_some(&mut body, "args", args.map(|a| json!(a)));
        set_if_some(
            &mut body,
            "protocol_version",
            protocol_version.map(str::to_string),
        );
        self.post("/api/machines", Some(body))
    }

    pub fn list_machines(&self) -> Result<Value, DaemonError> {
        self.get("/api/machines")
    }

    /// RAL-355 Phase 9: health of every configured `[machine.targets.*]`
    /// entry. Always live -- see `health_targets::check_all_targets`'s doc.
    pub fn health_remote_targets(&self) -> Result<Value, DaemonError> {
        self.get("/api/machines/targets/health")
    }

    pub fn get_machine(&self, scheme: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/machines/{scheme}"))
    }

    pub fn deregister_machine(&self, scheme: &str) -> Result<Value, DaemonError> {
        self.delete(&format!("/api/machines/{scheme}"))
    }

    /// Register (or update) a Triage type (RAL-318).
    pub fn register_triage_type(
        &self,
        name: &str,
        label: &str,
        description: &str,
    ) -> Result<Value, DaemonError> {
        self.post(
            "/api/triage/types",
            Some(json!({"name": name, "label": label, "description": description})),
        )
    }

    pub fn list_triage_types(&self) -> Result<Value, DaemonError> {
        self.get("/api/triage/types")
    }

    pub fn get_triage_type(&self, name: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/triage/types/{name}"))
    }

    pub fn deregister_triage_type(&self, name: &str) -> Result<Value, DaemonError> {
        self.delete(&format!("/api/triage/types/{name}"))
    }

    /// `ralphus triage pool list` (RAL-318): every `(project, triage_type)`
    /// pool key with pooled cells and/or a configured count threshold.
    pub fn list_triage_pools(&self) -> Result<Value, DaemonError> {
        self.get("/api/triage/pools")
    }

    /// `ralphus triage pool threshold` (RAL-318). `threshold: None` clears a
    /// previously configured threshold.
    pub fn set_triage_pool_threshold(
        &self,
        project: &str,
        triage_type: &str,
        threshold: Option<i64>,
    ) -> Result<Value, DaemonError> {
        self.post(
            "/api/triage/pools/threshold",
            Some(json!({"project": project, "triage_type": triage_type, "threshold": threshold})),
        )
    }

    /// `ralphus check health`'s live Arbiter round-trip (RAL-318).
    pub fn health_arbiter(&self) -> Result<Value, DaemonError> {
        self.post("/api/health/arbiter", None)
    }

    pub fn cleanup_machine(
        &self,
        machine: &str,
        project: &str,
        branch: Option<&str>,
    ) -> Result<Value, DaemonError> {
        self.post(
            "/api/machines/cleanup",
            Some(json!({"machine": machine, "project": project, "branch": branch})),
        )
    }

    pub fn clear(
        &self,
        states: Option<&[String]>,
        keep_temporary: bool,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({"keep_temporary": keep_temporary});
        set_if_some(&mut body, "states", states.map(|s| json!(s)));
        self.post("/api/clear", Some(body))
    }

    pub fn squad_graph(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/squads/{squad_id}/graph"))
    }

    pub fn global_graph(&self, include_terminal: bool) -> Result<Value, DaemonError> {
        let qs = if include_terminal { "?all=1" } else { "" };
        self.get(&format!("/api/graph{qs}"))
    }

    pub fn resources(&self) -> Result<Value, DaemonError> {
        self.get("/api/resources")
    }

    pub fn squad_worktrees(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/squads/{squad_id}/worktrees"))
    }

    pub fn squad_logs(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/squads/{squad_id}/logs"))
    }

    pub fn squad_timeline(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/squads/{squad_id}/timeline"))
    }

    /// `docs/daemon-api.md`'s `GET /api/cartographer` -- all filters optional.
    #[allow(clippy::too_many_arguments)]
    pub fn cartographer(&self, filters: CartographerFilters<'_>) -> Result<Value, DaemonError> {
        let qs = query_string(&[
            ("source", filters.source.map(str::to_string)),
            ("scope", filters.scope.map(str::to_string)),
            ("level", filters.level.map(str::to_string)),
            ("squad_id", filters.squad_id.map(str::to_string)),
            ("guardian_id", filters.guardian_id.map(str::to_string)),
            ("cell_id", filters.cell_id.map(str::to_string)),
            ("task", filters.task.map(str::to_string)),
            ("entity", filters.entity.map(str::to_string)),
            ("q", filters.q.map(str::to_string)),
            ("since_ms", filters.since_ms.map(|v| v.to_string())),
            ("until_ms", filters.until_ms.map(|v| v.to_string())),
            ("limit", Some(filters.limit.to_string())),
            ("offset", Some(filters.offset.to_string())),
            ("ascending", Some(filters.ascending.to_string())),
        ]);
        self.get(&format!("/api/cartographer{qs}"))
    }

    // ---- mailbox (RAL-241) ---------------------------------------------

    /// `POST /api/mailbox/register` -- returns `{"client_id": "..."}`.
    pub fn mailbox_register(&self) -> Result<Value, DaemonError> {
        self.post("/api/mailbox/register", None)
    }

    /// `GET /api/mailbox/{client_id}/messages` -- `priority` filters to one
    /// of `urgent`/`high`/`normal`.
    pub fn mailbox_messages(
        &self,
        client_id: &str,
        unread_only: bool,
        priority: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let qs = query_string(&[
            ("unread", unread_only.then(|| "true".to_string())),
            ("priority", priority.map(str::to_string)),
        ]);
        self.get(&format!("/api/mailbox/{client_id}/messages{qs}"))
    }

    /// `POST /api/mailbox/{client_id}/drain` -- `message_ids: None` drains
    /// every currently unread message; `Some(ids)` drains exactly those.
    pub fn mailbox_drain(
        &self,
        client_id: &str,
        message_ids: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({});
        set_if_some(&mut body, "message_ids", message_ids.map(|ids| json!(ids)));
        self.post(&format!("/api/mailbox/{client_id}/drain"), Some(body))
    }

    // ---- personal mailbox / follows / preferences (RAL-320) ------------

    /// `GET /api/mailbox/personal/messages` -- the acting user's personal
    /// mailbox, filtered through their follows. `user` omitted falls back to
    /// `.ralphus.toml`'s `default_user` daemon-side.
    pub fn personal_mailbox_messages(
        &self,
        unread_only: bool,
        priority: Option<&str>,
        user: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let qs = query_string(&[
            ("unread", unread_only.then(|| "true".to_string())),
            ("priority", priority.map(str::to_string)),
            ("user", user.map(str::to_string)),
        ]);
        self.get(&format!("/api/mailbox/personal/messages{qs}"))
    }

    /// `POST /api/mailbox/personal/drain` -- `message_ids: None` drains every
    /// currently unread message for the acting user; `Some(ids)` drains
    /// exactly those.
    pub fn personal_mailbox_drain(
        &self,
        message_ids: Option<&[String]>,
        user: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({});
        set_if_some(&mut body, "message_ids", message_ids.map(|ids| json!(ids)));
        let qs = query_string(&[("user", user.map(str::to_string))]);
        self.post(&format!("/api/mailbox/personal/drain{qs}"), Some(body))
    }

    /// `GET /api/follows` -- every follow the acting user owns.
    pub fn list_follows(&self, user: Option<&str>) -> Result<Value, DaemonError> {
        let qs = query_string(&[("user", user.map(str::to_string))]);
        self.get(&format!("/api/follows{qs}"))
    }

    /// `POST /api/follows` -- follow (or re-follow, updating tiers in place)
    /// an entity URI on the acting user's behalf. `notify_tiers` omitted or
    /// empty defaults to the acting user's `default_notify_tiers` preference.
    pub fn create_follow(
        &self,
        entity_uri: &str,
        notify_tiers: Option<&[String]>,
        user: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({"entity_uri": entity_uri});
        set_if_some(&mut body, "notify_tiers", notify_tiers.map(|t| json!(t)));
        let qs = query_string(&[("user", user.map(str::to_string))]);
        self.post(&format!("/api/follows{qs}"), Some(body))
    }

    /// `DELETE /api/follows/{entity_uri}` -- `entity_uri` is interpolated raw
    /// (not urlencoded): the daemon's route matcher expects the literal
    /// colon-delimited URI as the path segment.
    pub fn delete_follow(
        &self,
        entity_uri: &str,
        user: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let qs = query_string(&[("user", user.map(str::to_string))]);
        self.delete(&format!("/api/follows/{entity_uri}{qs}"))
    }

    /// `GET /api/users/{name}/preferences` -- unlike follows/personal
    /// mailbox, this endpoint has no `default_user` fallback, so `name` is
    /// required.
    pub fn get_user_preferences(&self, name: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/users/{name}/preferences"))
    }

    /// `POST /api/users/{name}/preferences`. `default_notify_tiers` omitted
    /// or empty means "every tier".
    pub fn set_user_preferences(
        &self,
        name: &str,
        auto_follow: bool,
        default_notify_tiers: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({"auto_follow": auto_follow});
        set_if_some(
            &mut body,
            "default_notify_tiers",
            default_notify_tiers.map(|t| json!(t)),
        );
        self.post(&format!("/api/users/{name}/preferences"), Some(body))
    }

    pub fn activate_squad(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/squads/{squad_id}/activate"), None)
    }

    pub fn retry_squad(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/squads/{squad_id}/retry"), None)
    }

    pub fn restart_squad(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/squads/{squad_id}/restart"), None)
    }

    pub fn add_dependency(&self, squad_id: &str, target_id: &str) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/add-dependency"),
            Some(json!({"target_id": target_id})),
        )
    }

    pub fn cell_pane(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        lines: i64,
    ) -> Result<Value, DaemonError> {
        self.get(&format!(
            "/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/pane?lines={lines}"
        ))
    }

    pub fn proof_pane(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        proof_idx: i64,
        lines: i64,
    ) -> Result<Value, DaemonError> {
        self.get(&format!(
            "/api/squads/{squad_id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/pane?lines={lines}"
        ))
    }

    /// This cell's merged, current-attempt-only debug stream (RAL-296) —
    /// bare JSON array of `SquadTimelineEntry`, ascending by time.
    pub fn cell_debug_events(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<Value, DaemonError> {
        self.get(&format!(
            "/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/debug-events"
        ))
    }

    /// Same as [`Self::cell_debug_events`], for a `prompt`-kind proof step.
    pub fn proof_debug_events(
        &self,
        squad_id: &str,
        task_idx: i64,
        scope: &str,
        cell_idx: i64,
        proof_idx: i64,
    ) -> Result<Value, DaemonError> {
        self.get(&format!(
            "/api/squads/{squad_id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/debug-events"
        ))
    }

    pub fn ghost_get(&self, owner_uri: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/ghosts/{}", urlencode(owner_uri)))
    }

    pub fn restart_cell(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/restart"),
            None,
        )
    }

    /// `POST .../cells/{ti}/{si}/open-terminal?mode=agent` (RAL-288 Stage
    /// 6): while the cell is running, cleanly detaches it and opens the
    /// real interactive agent in a new terminal (tmux-wrapped, survives
    /// closing this window); once finished, resumes it the old way. Same
    /// endpoint either way -- the daemon decides which based on the cell's
    /// current state.
    pub fn open_agent_terminal(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/open-terminal?mode=agent"),
            None,
        )
    }

    /// `POST .../cells/{ti}/{si}/terminal-ticket` (RAL-355 Phase 10): mints a
    /// one-shot ticket for the remote Open Agent terminal relay -- the
    /// WebSocket counterpart to [`Self::open_agent_terminal`] for a cell
    /// running on a `machine`, since that route only ever spawns a terminal
    /// window on the daemon's own desktop. Returns `{"ticket", "port",
    /// "path"}`; the caller connects the actual WebSocket itself (see
    /// `commands/terminal_relay.rs`) -- this client has no WS transport of
    /// its own, matching every other method here being plain HTTP/JSON.
    pub fn mint_terminal_ticket(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/terminal-ticket"),
            None,
        )
    }

    /// `POST .../cells/{ti}/{si}/resume-automation` (RAL-288 Stage 6): hands
    /// a detached cell back to unattended execution, continuing the exact
    /// same agent conversation rather than starting fresh. Rejected if the
    /// cell is actually still live (not detached), rather than racing it.
    pub fn resume_automation(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/resume-automation"),
            None,
        )
    }

    pub fn restart_cell_proof(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        proof_idx: i64,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!(
                "/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/proof/{proof_idx}/restart"
            ),
            None,
        )
    }

    pub fn restart_task_proof(
        &self,
        squad_id: &str,
        task_idx: i64,
        proof_idx: i64,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/tasks/{task_idx}/proof/{proof_idx}/restart"),
            None,
        )
    }

    pub fn restart_task(&self, squad_id: &str, task_idx: i64) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/tasks/{task_idx}/restart"),
            None,
        )
    }

    // ---- env overrides -----------------------------------------------------

    fn env_payload(set_vars: Option<&Value>, unset_vars: Option<&[String]>) -> Value {
        let mut body = json!({});
        set_if_some(&mut body, "set", set_vars.cloned());
        set_if_some(&mut body, "unset", unset_vars.map(|v| json!(v)));
        body
    }

    pub fn set_squad_env(
        &self,
        squad_id: &str,
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/env"),
            Some(Self::env_payload(set_vars, unset_vars)),
        )
    }

    pub fn set_task_env(
        &self,
        squad_id: &str,
        task_idx: i64,
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/tasks/{task_idx}/env"),
            Some(Self::env_payload(set_vars, unset_vars)),
        )
    }

    pub fn set_task_proof_env(
        &self,
        squad_id: &str,
        task_idx: i64,
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/tasks/{task_idx}/proof/env"),
            Some(Self::env_payload(set_vars, unset_vars)),
        )
    }

    pub fn set_cell_env(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/env"),
            Some(Self::env_payload(set_vars, unset_vars)),
        )
    }

    pub fn set_cell_proof_env(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/proof/env"),
            Some(Self::env_payload(set_vars, unset_vars)),
        )
    }

    pub fn set_task_proof_step_env(
        &self,
        squad_id: &str,
        task_idx: i64,
        proof_idx: i64,
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/tasks/{task_idx}/proof/{proof_idx}/env"),
            Some(Self::env_payload(set_vars, unset_vars)),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_cell_proof_step_env(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        proof_idx: i64,
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/cells/{task_idx}/{cell_idx}/proof/{proof_idx}/env"),
            Some(Self::env_payload(set_vars, unset_vars)),
        )
    }

    // ---- edit / delete -------------------------------------------------

    pub fn edit_squad(&self, squad_id: &str, label: &str) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/squads/{squad_id}/edit"),
            Some(json!({"kind": "squad", "label": label})),
        )
    }

    pub fn edit_task(
        &self,
        squad_id: &str,
        task_idx: i64,
        name: Option<&str>,
        project: Option<&str>,
        model: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({"kind": "task", "task_idx": task_idx});
        set_if_some(&mut body, "name", name.map(str::to_string));
        set_if_some(&mut body, "project", project.map(str::to_string));
        set_if_some(&mut body, "model", model.map(str::to_string));
        self.post(&format!("/api/squads/{squad_id}/edit"), Some(body))
    }

    pub fn edit_proof(
        &self,
        squad_id: &str,
        task_idx: i64,
        proof_scope: &str,
        cell_idx: i64,
        proof_idx: i64,
        model: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({
            "kind": "proof",
            "task_idx": task_idx,
            "proof_scope": proof_scope,
            "cell_idx": cell_idx,
            "proof_idx": proof_idx,
        });
        set_if_some(&mut body, "model", model.map(str::to_string));
        self.post(&format!("/api/squads/{squad_id}/edit"), Some(body))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn edit_cell(
        &self,
        squad_id: &str,
        task_idx: i64,
        cell_idx: i64,
        cwd: Option<&str>,
        agent: Option<&str>,
        model: Option<&str>,
        prompt: Option<&str>,
        command: Option<&str>,
        auto_compact_threshold: Option<&str>,
        system_prompt: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({"kind": "cell", "task_idx": task_idx, "cell_idx": cell_idx});
        set_if_some(&mut body, "cwd", cwd.map(str::to_string));
        set_if_some(&mut body, "agent", agent.map(str::to_string));
        set_if_some(&mut body, "model", model.map(str::to_string));
        set_if_some(&mut body, "prompt", prompt.map(str::to_string));
        set_if_some(&mut body, "command", command.map(str::to_string));
        set_if_some(
            &mut body,
            "auto_compact_threshold",
            auto_compact_threshold.map(str::to_string),
        );
        set_if_some(
            &mut body,
            "system_prompt",
            system_prompt.map(str::to_string),
        );
        self.post(&format!("/api/squads/{squad_id}/edit"), Some(body))
    }

    pub fn delete_squad(&self, squad_id: &str) -> Result<Value, DaemonError> {
        self.delete(&format!("/api/squads/{squad_id}"))
    }

    // ---- guardians (reviews) -------------------------------------------

    pub fn guardian_list(&self) -> Result<Value, DaemonError> {
        self.get("/api/guardians")
    }

    pub fn guardian_get(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/guardians/{guardian_id}"))
    }

    pub fn guardian_logs(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/guardians/{guardian_id}/logs"))
    }

    /// `clear_vars` is a list of specific keys to clear, matching the
    /// daemon's actual `InheritedEnvOverridesBody` contract
    /// (`daemon/src/server.rs::set_guardian_scoped_env`) -- `set`/`unset`/
    /// `clear` are all key lists, not a blanket boolean.
    fn guardian_env_payload(
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
        clear_vars: Option<&[String]>,
    ) -> Value {
        let mut body = json!({});
        set_if_some(&mut body, "set", set_vars.cloned());
        set_if_some(&mut body, "unset", unset_vars.map(|v| json!(v)));
        set_if_some(&mut body, "clear", clear_vars.map(|v| json!(v)));
        body
    }

    pub fn set_guardian_build_env(
        &self,
        guardian_id: &str,
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
        clear_vars: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/build-env"),
            Some(Self::guardian_env_payload(set_vars, unset_vars, clear_vars)),
        )
    }

    pub fn set_guardian_manual_checks_env(
        &self,
        guardian_id: &str,
        set_vars: Option<&Value>,
        unset_vars: Option<&[String]>,
        clear_vars: Option<&[String]>,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/manual-checks-env"),
            Some(Self::guardian_env_payload(set_vars, unset_vars, clear_vars)),
        )
    }

    /// `POST /api/guardians/{id}/squash` (RAL-91): toggles per-commit
    /// squashing for one git project within a review. Was missing from this
    /// client despite the daemon endpoint being real and the Python CLI's
    /// `review squash` command already calling a same-named (but
    /// never-defined) method on the Python `DaemonClient` -- an existing bug
    /// there (`AttributeError` on every invocation), not a design decision
    /// to preserve.
    pub fn guardian_squash(
        &self,
        guardian_id: &str,
        project: &str,
        enabled: bool,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/squash"),
            Some(json!({"project": project, "enabled": enabled})),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn guardian_create(
        &self,
        name: &str,
        base_branch: &str,
        git_root: &str,
        checks: Option<&[String]>,
        skip_auto_build: bool,
        skip_worktrees: bool,
        review_type: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({
            "name": name, "base_branch": base_branch, "git_root": git_root,
            "skip_auto_build": skip_auto_build,
            "skip_worktrees": skip_worktrees,
        });
        set_if_some(&mut body, "checks", checks.map(|c| json!(c)));
        set_if_some(&mut body, "review_type", review_type.map(str::to_string));
        self.post("/api/guardians", Some(body))
    }

    pub fn guardian_rename(&self, guardian_id: &str, name: &str) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/rename"),
            Some(json!({"name": name})),
        )
    }

    /// `GuardianSettings` bundles the ten optional settings fields so this
    /// method's signature doesn't grow another positional parameter.
    pub fn guardian_settings(
        &self,
        guardian_id: &str,
        settings: &GuardianSettings<'_>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({});
        set_if_some(&mut body, "skip_auto_build", settings.skip_auto_build);
        set_if_some(&mut body, "skip_worktrees", settings.skip_worktrees);
        set_if_some(
            &mut body,
            "resolver_agent",
            settings.resolver_agent.map(str::to_string),
        );
        set_if_some(
            &mut body,
            "resolver_model",
            settings.resolver_model.map(str::to_string),
        );
        set_if_some(
            &mut body,
            "base_branch",
            settings.base_branch.map(str::to_string),
        );
        set_if_some(&mut body, "auto_pr_feedback", settings.auto_pr_feedback);
        set_if_some(
            &mut body,
            "proof_scope",
            settings.proof_scope.map(str::to_string),
        );
        set_if_some(
            &mut body,
            "proof_skip_auto_clean",
            settings.proof_skip_auto_clean,
        );
        set_if_some(&mut body, "skip_base_updates", settings.skip_base_updates);
        set_if_some(
            &mut body,
            "match_pr_branch_name",
            settings.match_pr_branch_name,
        );
        set_if_some(
            &mut body,
            "auto_submit_pr_stack",
            settings.auto_submit_pr_stack,
        );
        self.post(
            &format!("/api/guardians/{guardian_id}/settings"),
            Some(body),
        )
    }

    pub fn guardian_delete(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.delete(&format!("/api/guardians/{guardian_id}"))
    }

    pub fn guardian_add_branch(
        &self,
        guardian_id: &str,
        branch: &str,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/branches"),
            Some(json!({"branch": branch})),
        )
    }

    pub fn guardian_arrange(
        &self,
        guardian_id: &str,
        order: &[String],
        enabled: Option<&Value>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({"order": order});
        set_if_some(&mut body, "enabled", enabled.cloned());
        self.post(
            &format!("/api/guardians/{guardian_id}/branches/arrange"),
            Some(body),
        )
    }

    pub fn guardian_feedback(
        &self,
        guardian_id: &str,
        branch_id: &str,
        feedback: &str,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/branches/{branch_id}/feedback"),
            Some(json!({"feedback": feedback})),
        )
    }

    pub fn guardian_base_branches(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/guardians/{guardian_id}/base-branches"))
    }

    pub fn guardian_change_base(
        &self,
        guardian_id: &str,
        branch: &str,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/base"),
            Some(json!({"branch": branch})),
        )
    }

    pub fn guardian_force_start(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/guardians/{guardian_id}/force_start"), None)
    }

    pub fn guardian_dismiss_reenable(
        &self,
        guardian_id: &str,
        branch_id: &str,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/branches/{branch_id}/dismiss_reenable"),
            None,
        )
    }

    pub fn guardian_move_branch(
        &self,
        guardian_id: &str,
        branch_id: &str,
        to_guardian_id: &str,
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/branches/{branch_id}/move"),
            Some(json!({"to_guardian_id": to_guardian_id})),
        )
    }

    pub fn guardian_merge(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/guardians/{guardian_id}/merge"), None)
    }

    pub fn guardian_sync_pr(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/guardians/{guardian_id}/sync-pr"), None)
    }

    pub fn guardian_cancel_and_merge(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/cancel_and_merge"),
            None,
        )
    }

    /// Stop an in-progress rebase at its next checkpoint, leaving the review in
    /// the recoverable `merge_stopped` state (RAL-249), distinct from cancel.
    pub fn guardian_stop(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/guardians/{guardian_id}/stop"), None)
    }

    pub fn guardian_approve(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/guardians/{guardian_id}/approve"), None)
    }

    pub fn guardian_cancel(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/guardians/{guardian_id}/cancel"), None)
    }

    /// Reopen a `cancelled` review (status → `collecting`) and immediately try
    /// a fresh merge pass if the daemon has capacity.
    pub fn guardian_reopen(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/guardians/{guardian_id}/reopen"), None)
    }

    pub fn guardian_submit_prs(
        &self,
        guardian_id: &str,
        prs: &[Value],
    ) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/pull-requests"),
            Some(json!({"prs": prs})),
        )
    }

    pub fn guardian_list_prs(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/guardians/{guardian_id}/pull-requests"))
    }

    /// RAL-317: bulk-drop every currently open PR row for a guardian and
    /// clear its registered forge PR stack number (`review pr unlink`).
    pub fn guardian_unlink_prs(&self, guardian_id: &str) -> Result<Value, DaemonError> {
        self.post(
            &format!("/api/guardians/{guardian_id}/pull-requests/unlink"),
            None,
        )
    }

    // ---- pull requests ---------------------------------------------------

    pub fn pr_get(&self, pr_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/pull-requests/{pr_id}"))
    }

    pub fn pr_find(&self, forge: &str, repo: &str, pr_number: i64) -> Result<Value, DaemonError> {
        let qs = query_string(&[
            ("forge", Some(forge.to_string())),
            ("repo", Some(repo.to_string())),
            ("pr_number", Some(pr_number.to_string())),
        ]);
        self.get(&format!("/api/pull-requests{qs}"))
    }

    pub fn pr_update(
        &self,
        pr_id: &str,
        pr_number: Option<i64>,
        pr_url: Option<&str>,
        branch_alias: Option<&str>,
        state: Option<&str>,
    ) -> Result<Value, DaemonError> {
        let mut body = json!({});
        set_if_some(&mut body, "pr_number", pr_number);
        set_if_some(&mut body, "pr_url", pr_url.map(str::to_string));
        set_if_some(&mut body, "branch_alias", branch_alias.map(str::to_string));
        set_if_some(&mut body, "state", state.map(str::to_string));
        self.post(&format!("/api/pull-requests/{pr_id}"), Some(body))
    }

    pub fn pr_comments(&self, pr_id: &str) -> Result<Value, DaemonError> {
        self.get(&format!("/api/pull-requests/{pr_id}/comments"))
    }

    pub fn pr_action_feedback(&self, pr_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/pull-requests/{pr_id}/action-feedback"), None)
    }

    pub fn pr_pull_from_pr(&self, pr_id: &str) -> Result<Value, DaemonError> {
        self.post(&format!("/api/pull-requests/{pr_id}/pull-from-pr"), None)
    }
}

/// Optional filters for [`DaemonClient::cartographer`] -- bundled to avoid a
/// 13-parameter method signature.
#[derive(Debug, Clone)]
pub struct CartographerFilters<'a> {
    pub source: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub level: Option<&'a str>,
    pub squad_id: Option<&'a str>,
    pub guardian_id: Option<&'a str>,
    pub cell_id: Option<&'a str>,
    pub task: Option<&'a str>,
    pub entity: Option<&'a str>,
    pub q: Option<&'a str>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub limit: i64,
    pub offset: i64,
    pub ascending: bool,
}

impl Default for CartographerFilters<'_> {
    fn default() -> Self {
        Self {
            source: None,
            scope: None,
            level: None,
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            entity: None,
            q: None,
            since_ms: None,
            until_ms: None,
            limit: 100,
            offset: 0,
            ascending: false,
        }
    }
}

/// Bundled optional fields for [`DaemonClient::guardian_settings`].
#[derive(Debug, Clone, Default)]
pub struct GuardianSettings<'a> {
    pub skip_auto_build: Option<bool>,
    pub skip_worktrees: Option<bool>,
    pub resolver_agent: Option<&'a str>,
    pub resolver_model: Option<&'a str>,
    pub base_branch: Option<&'a str>,
    pub auto_pr_feedback: Option<bool>,
    pub proof_scope: Option<&'a str>,
    pub proof_skip_auto_clean: Option<bool>,
    /// RAL-250: whether this review skips automatic base-branch auto-updates.
    pub skip_base_updates: Option<bool>,
    /// RAL-307: whether this review defaults a newly submitted PR's branch
    /// to the exact worktree/feature branch name.
    pub match_pr_branch_name: Option<bool>,
    /// RAL-317: whether this review's PR stack is auto-submitted/grown as
    /// each branch reaches a terminal merge state.
    pub auto_submit_pr_stack: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TestServer {
        server: Arc<tiny_http::Server>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        /// Spins up a real ephemeral local HTTP server -- `ureq` has no
        /// pluggable-transport mock the way `httpx.MockTransport` does, so
        /// tests exercise the real request/response path against a
        /// throwaway `tiny_http` server instead (already a workspace
        /// dependency, used the same way by the daemon's own test suite).
        fn start(respond: impl Fn(&tiny_http::Request) -> (u16, String) + Send + 'static) -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let server_for_thread = Arc::clone(&server);
            let handle = std::thread::spawn(move || {
                if let Ok(Some(request)) = server_for_thread.recv_timeout(Duration::from_secs(5)) {
                    let (status, body) = respond(&request);
                    let header = tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"application/json"[..],
                    )
                    .unwrap();
                    let response = tiny_http::Response::from_string(body)
                        .with_status_code(status)
                        .with_header(header);
                    let _ = request.respond(response);
                }
            });
            Self {
                server,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!(
                "http://127.0.0.1:{}",
                self.server.server_addr().to_ip().unwrap().port()
            )
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[test]
    fn health_reaches_daemon_and_parses_json() {
        let server = TestServer::start(|_req| (200, r#"{"status":"ok"}"#.to_string()));
        let client = DaemonClient::new(server.url());
        let result = client.health().unwrap();
        assert_eq!(result["status"], "ok");
    }

    #[test]
    fn guardian_squash_posts_to_the_squash_endpoint() {
        let server = TestServer::start(|req| {
            assert_eq!(req.url(), "/api/guardians/g1/squash");
            assert_eq!(req.method(), &tiny_http::Method::Post);
            (200, r#"{"id":"g1"}"#.to_string())
        });
        let client = DaemonClient::new(server.url());
        let result = client.guardian_squash("g1", "proj", true).unwrap();
        assert_eq!(result["id"], "g1");
    }

    #[test]
    fn error_envelope_maps_to_daemon_error_with_status_code() {
        let server = TestServer::start(|_req| {
            (
                404,
                r#"{"error":{"code":"not_found","message":"no such squad"}}"#.to_string(),
            )
        });
        let client = DaemonClient::new(server.url());
        let err = client.squad("missing").unwrap_err();
        assert_eq!(err.status_code, Some(404));
        assert_eq!(err.message, "no such squad");
    }

    #[test]
    fn unreachable_daemon_yields_none_status_code() {
        // Nothing listening on this port.
        let client = DaemonClient::with_timeout("http://127.0.0.1:1", Duration::from_millis(200));
        let err = client.health().unwrap_err();
        assert_eq!(err.status_code, None);
    }

    #[test]
    fn guardian_response_env_values_are_redacted() {
        let server = TestServer::start(|_req| {
            (
                200,
                r#"{"id":"g1","combined_env":{"SECRET":"abc123","OTHER":"value"},"branches":[{"env_overrides":{"A":"1","B":null}}]}"#
                    .to_string(),
            )
        });
        let client = DaemonClient::new(server.url());
        let result = client.guardian_get("g1").unwrap();
        assert_eq!(result["combined_env"]["SECRET"], "<hidden>");
        assert_eq!(result["combined_env"]["OTHER"], "<hidden>");
        assert_eq!(result["branches"][0]["env_overrides"]["A"], "<hidden>");
        // Tombstone (explicit unset) markers must survive redaction.
        assert!(result["branches"][0]["env_overrides"]["B"].is_null());
    }

    #[test]
    fn non_guardian_response_is_not_redacted() {
        let server =
            TestServer::start(|_req| (200, r#"{"combined_env":{"SECRET":"abc123"}}"#.to_string()));
        let client = DaemonClient::new(server.url());
        let result = client.squad("r1").unwrap();
        assert_eq!(result["combined_env"]["SECRET"], "abc123");
    }

    #[test]
    fn query_string_skips_none_and_encodes_values() {
        assert_eq!(
            query_string(&[("a", Some("b c".to_string())), ("d", None)]),
            "?a=b%20c"
        );
        assert_eq!(query_string(&[("a", None)]), "");
    }
}
