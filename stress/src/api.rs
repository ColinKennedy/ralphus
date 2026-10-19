//! HTTP client side: the operation catalogue, request execution with outcome
//! classification, and the capability probe that adapts to whichever daemon
//! version is under test (`/api/squads` vs the older `/api/runs`, with or
//! without bearer tokens and registered users).

use crate::stats::Outcome;
use serde::Serialize;
use std::io::Read;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// One kind of request a virtual user can make.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Health,
    Board,
    SquadGet,
    Queue,
    Projects,
    TaskIndex,
    Guardians,
    Submit,
    EnvSet,
    LabelEdit,
    Validate,
}

impl Op {
    pub const ALL: [Op; 11] = [
        Op::Health,
        Op::Board,
        Op::SquadGet,
        Op::Queue,
        Op::Projects,
        Op::TaskIndex,
        Op::Guardians,
        Op::Submit,
        Op::EnvSet,
        Op::LabelEdit,
        Op::Validate,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Op::Health => "health",
            Op::Board => "board",
            Op::SquadGet => "squad_get",
            Op::Queue => "queue",
            Op::Projects => "projects",
            Op::TaskIndex => "task_index",
            Op::Guardians => "guardians",
            Op::Submit => "submit",
            Op::EnvSet => "env_set",
            Op::LabelEdit => "label_edit",
            Op::Validate => "validate",
        }
    }
}

/// The result of one request: how it ended, how many body bytes came back,
/// the body itself when the caller asked for it, and the error text when it
/// failed.
pub struct Reply {
    pub outcome: Outcome,
    pub bytes: u64,
    pub body: Option<String>,
    pub error: Option<String>,
}

/// Everything a virtual user needs to talk to one daemon instance.
pub struct Api {
    pub base: String,
    pub token: Option<String>,
    /// `squads` on current daemons, `runs` before the RAL-239 rename.
    pub noun: String,
    /// The response key holding a new squad's id (`squad_id`/`run_id`/`id`).
    pub id_key: String,
    /// `kind` value for a label-only edit (`squad` or `run`).
    pub edit_kind: String,
    /// Registered user names; empty when the daemon has no user registry.
    pub users: Vec<String>,
    pub available: Vec<Op>,
    pub unavailable: Vec<(Op, String)>,
    /// Cell working directories to submit against (registered project repos
    /// when there are any).
    pub cwds: RwLock<Vec<String>>,
    pub ids: RwLock<Vec<String>>,
    pub counter: AtomicU64,
    pub timeout: Duration,
}

pub fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(timeout)
        .max_idle_connections_per_host(1)
        .build()
}

fn classify_transport(t: &ureq::Transport) -> Outcome {
    let msg = t.to_string().to_lowercase();
    if msg.contains("timed out") || msg.contains("would block") {
        return Outcome::Timeout;
    }
    match t.kind() {
        ureq::ErrorKind::ConnectionFailed => Outcome::Connect,
        ureq::ErrorKind::BadStatus | ureq::ErrorKind::BadHeader => Outcome::Protocol,
        _ if msg.contains("reset")
            || msg.contains("forcibly closed")
            || msg.contains("aborted")
            || msg.contains("broken pipe")
            || msg.contains("unexpected eof")
            || msg.contains("connection closed") =>
        {
            Outcome::Reset
        }
        _ => Outcome::Io,
    }
}

fn classify_io(e: &std::io::Error) -> Outcome {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::TimedOut | K::WouldBlock => Outcome::Timeout,
        K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe | K::UnexpectedEof => {
            Outcome::Reset
        }
        _ => Outcome::Io,
    }
}

/// Read the whole body (so the keep-alive connection stays reusable) and
/// optionally keep it.
fn drain(resp: ureq::Response, want_body: bool) -> Result<(u64, Option<String>), std::io::Error> {
    let mut reader = resp.into_reader();
    if want_body {
        let mut s = String::new();
        let n = reader.read_to_string(&mut s)?;
        Ok((n as u64, Some(s)))
    } else {
        let n = std::io::copy(&mut reader, &mut std::io::sink())?;
        Ok((n, None))
    }
}

/// Execute one request against `base`. Shared by the probe (before an `Api`
/// exists) and by `Api::call`.
#[allow(clippy::too_many_arguments)]
pub fn raw_call(
    agent: &ureq::Agent,
    base: &str,
    token: Option<&str>,
    user: Option<&str>,
    method: &str,
    path: &str,
    body: Option<&str>,
    want_body: bool,
) -> Reply {
    let mut req = agent.request(method, &format!("{base}{path}"));
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    if let Some(u) = user {
        req = req.set("X-Ralphus-User", u);
    }
    let result = match body {
        Some(b) => req.set("Content-Type", "application/json").send_string(b),
        None if method == "GET" || method == "DELETE" => req.call(),
        None => req.send_string(""),
    };
    match result {
        Ok(resp) => {
            let status = resp.status();
            match drain(resp, want_body) {
                Ok((bytes, body)) => Reply {
                    outcome: Outcome::Ok(status),
                    bytes,
                    body,
                    error: None,
                },
                Err(e) => Reply {
                    outcome: classify_io(&e),
                    bytes: 0,
                    body: None,
                    error: Some(format!("{method} {path}: reading body: {e}")),
                },
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = drain(resp, true)
                .ok()
                .and_then(|(_, b)| b)
                .unwrap_or_default();
            let snippet: String = text.chars().take(300).collect();
            Reply {
                outcome: Outcome::Http(code),
                bytes: text.len() as u64,
                body: Some(text),
                error: Some(format!("{method} {path} -> {code}: {snippet}")),
            }
        }
        Err(ureq::Error::Transport(t)) => Reply {
            outcome: classify_transport(&t),
            bytes: 0,
            body: None,
            error: Some(format!("{method} {path}: {t}")),
        },
    }
}

/// A tiny xorshift generator: the workspace has no `rand`, and load
/// generation needs speed and spread, not cryptographic quality.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// A one-task, one-cell squad. A `prompt` cell is the shape every daemon
/// version accepts without extra fields (a `command` cell needs
/// `remediation_attempts` on current ones); it is submitted held, so it never
/// runs.
pub fn submit_toml(n: u64, cwd: &str) -> String {
    format!(
        "[[task]]\nname = \"stress-{n}\"\n[[task.cell]]\ncwd = \"{cwd}\"\nprompt = \"stress {n}\"\n"
    )
}

fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

pub fn submit_body(n: u64, cwd: &str) -> String {
    format!(
        "{{\"toml\":{},\"hold\":true,\"label\":\"stress-{n}\"}}",
        json_str(&submit_toml(n, cwd))
    )
}

fn extract_id(body: &str, key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

impl Api {
    pub fn user_for(&self, vu: usize) -> Option<&str> {
        if self.users.is_empty() {
            None
        } else {
            Some(self.users[vu % self.users.len()].as_str())
        }
    }

    pub fn call(
        &self,
        agent: &ureq::Agent,
        user: Option<&str>,
        method: &str,
        path: &str,
        body: Option<&str>,
        want_body: bool,
    ) -> Reply {
        raw_call(
            agent,
            &self.base,
            self.token.as_deref(),
            user,
            method,
            path,
            body,
            want_body,
        )
    }

    fn random_id(&self, rng: &mut Rng) -> Option<String> {
        let ids = self.ids.read().ok()?;
        if ids.is_empty() {
            return None;
        }
        Some(ids[rng.below(ids.len())].clone())
    }

    fn random_cwd(&self, rng: &mut Rng) -> String {
        match self.cwds.read() {
            Ok(c) if !c.is_empty() => c[rng.below(c.len())].clone(),
            _ => "C:/stress-cwd".to_string(),
        }
    }

    pub fn submit(&self, agent: &ureq::Agent, user: Option<&str>, rng: &mut Rng) -> Reply {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let cwd = self.random_cwd(rng);
        let body = submit_body(n, &cwd);
        let path = format!("/api/{}", self.noun);
        let mut reply = self.call(agent, user, "POST", &path, Some(&body), true);
        if reply.outcome.is_ok() {
            match reply
                .body
                .as_deref()
                .and_then(|b| extract_id(b, &self.id_key))
            {
                Some(id) => {
                    if let Ok(mut ids) = self.ids.write() {
                        ids.push(id);
                    }
                }
                None => {
                    reply.error = Some(format!(
                        "submit returned no {:?}: {:?}",
                        self.id_key, reply.body
                    ));
                    reply.outcome = Outcome::Protocol;
                }
            }
        }
        reply
    }

    /// Run one `op`. Ops that need an existing squad fall back to a board
    /// read when none exists yet.
    pub fn exec(&self, agent: &ureq::Agent, user: Option<&str>, op: Op, rng: &mut Rng) -> Reply {
        let noun = &self.noun;
        match op {
            Op::Health => self.call(agent, user, "GET", "/api/daemon", None, false),
            Op::Board => self.call(agent, user, "GET", "/api/tasks", None, false),
            Op::Queue => self.call(agent, user, "GET", "/api/queue", None, false),
            Op::Projects => self.call(agent, user, "GET", "/api/projects", None, false),
            Op::TaskIndex => self.call(agent, user, "GET", "/api/task-index", None, false),
            Op::Guardians => self.call(agent, user, "GET", "/api/guardians", None, false),
            Op::Submit => self.submit(agent, user, rng),
            Op::Validate => {
                let n = self.counter.fetch_add(1, Ordering::Relaxed);
                let body = format!(
                    "{{\"toml\":{}}}",
                    json_str(&submit_toml(n, &self.random_cwd(rng)))
                );
                self.call(
                    agent,
                    user,
                    "POST",
                    &format!("/api/{noun}/validate"),
                    Some(&body),
                    false,
                )
            }
            Op::SquadGet | Op::EnvSet | Op::LabelEdit => {
                let Some(id) = self.random_id(rng) else {
                    return self.call(agent, user, "GET", "/api/tasks", None, false);
                };
                let n = self.counter.fetch_add(1, Ordering::Relaxed);
                match op {
                    Op::SquadGet => self.call(
                        agent,
                        user,
                        "GET",
                        &format!("/api/{noun}/{id}"),
                        None,
                        false,
                    ),
                    Op::EnvSet => {
                        let body = format!("{{\"set\":{{\"STRESS_K\":\"v{n}\"}}}}");
                        self.call(
                            agent,
                            user,
                            "POST",
                            &format!("/api/{noun}/{id}/env"),
                            Some(&body),
                            false,
                        )
                    }
                    _ => {
                        let body = format!(
                            "{{\"kind\":\"{}\",\"label\":\"stress-edit-{n}\"}}",
                            self.edit_kind
                        );
                        self.call(
                            agent,
                            user,
                            "POST",
                            &format!("/api/{noun}/{id}/edit"),
                            Some(&body),
                            false,
                        )
                    }
                }
            }
        }
    }
}

/// What the probe learned about the daemon, recorded in the results so
/// numbers from different versions can be compared honestly.
#[derive(Serialize, Clone, Debug)]
pub struct Capabilities {
    pub noun: String,
    pub auth: bool,
    pub users: bool,
    pub available_ops: Vec<Op>,
    pub unavailable_ops: Vec<(Op, String)>,
}

impl Api {
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            noun: self.noun.clone(),
            auth: self.token.is_some(),
            users: !self.users.is_empty(),
            available_ops: self.available.clone(),
            unavailable_ops: self.unavailable.clone(),
        }
    }
}

/// Discover the API shape of the daemon at `base` and register `user_count`
/// users (when the daemon has a user registry).
pub fn probe(
    base: &str,
    token: Option<String>,
    user_count: usize,
    cwd: &str,
    timeout: Duration,
) -> Result<Api, String> {
    let ag = agent(timeout);
    let tok = token.as_deref();

    // Users: register one; 404/405 means this daemon predates the registry.
    let mut users = Vec::new();
    let first = raw_call(
        &ag,
        base,
        tok,
        None,
        "POST",
        "/api/users",
        Some("{\"name\":\"stress-0\"}"),
        false,
    );
    let has_users = matches!(first.outcome, Outcome::Ok(_) | Outcome::Http(409));
    if has_users {
        users.push("stress-0".to_string());
        for i in 1..user_count {
            let name = format!("stress-{i}");
            let r = raw_call(
                &ag,
                base,
                tok,
                None,
                "POST",
                "/api/users",
                Some(&format!("{{\"name\":\"{name}\"}}")),
                false,
            );
            if matches!(r.outcome, Outcome::Ok(_) | Outcome::Http(409)) {
                users.push(name);
            } else {
                return Err(format!(
                    "registering user {name}: {}",
                    r.error.unwrap_or_default()
                ));
            }
        }
    }
    let first_user = users.first().cloned();
    let user = first_user.as_deref();

    // Submit noun.
    let mut found = None;
    for noun in ["squads", "runs"] {
        let r = raw_call(
            &ag,
            base,
            tok,
            user,
            "POST",
            &format!("/api/{noun}"),
            Some(&submit_body(0, cwd)),
            true,
        );
        if r.outcome.is_ok() {
            let body = r.body.unwrap_or_default();
            let key = ["squad_id", "run_id", "id"]
                .into_iter()
                .find(|k| extract_id(&body, k).is_some())
                .ok_or_else(|| format!("submit to /api/{noun} returned no id: {body}"))?;
            let id = extract_id(&body, key).unwrap_or_default();
            found = Some((noun.to_string(), key.to_string(), id));
            break;
        }
        if !matches!(r.outcome, Outcome::Http(404) | Outcome::Http(405)) {
            return Err(format!(
                "probe submit to /api/{noun} failed: {}",
                r.error.unwrap_or_default()
            ));
        }
    }
    let (noun, id_key, first_id) =
        found.ok_or("neither POST /api/squads nor /api/runs accepted a submit")?;

    let mut api = Api {
        base: base.to_string(),
        token,
        noun: noun.clone(),
        id_key,
        edit_kind: if noun == "runs" {
            "run".into()
        } else {
            "squad".into()
        },
        users,
        available: Vec::new(),
        unavailable: Vec::new(),
        cwds: RwLock::new(vec![cwd.to_string()]),
        ids: RwLock::new(vec![first_id]),
        counter: AtomicU64::new(1),
        timeout,
    };

    let mut rng = Rng::new(7);
    for op in Op::ALL {
        let mut r = api.exec(&ag, user, op, &mut rng);
        if op == Op::LabelEdit && !r.outcome.is_ok() && api.edit_kind == "squad" {
            api.edit_kind = "run".into();
            r = api.exec(&ag, user, op, &mut rng);
            if !r.outcome.is_ok() {
                api.edit_kind = "squad".into();
            }
        }
        if r.outcome.is_ok() {
            api.available.push(op);
        } else {
            api.unavailable
                .push((op, r.error.unwrap_or_else(|| r.outcome.class())));
        }
    }
    Ok(api)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_body_is_valid_json_with_hold() {
        let body = submit_body(3, "C:/x y/\"q\"");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["hold"], true);
        assert!(v["toml"].as_str().unwrap().contains("stress-3"));
    }

    #[test]
    fn extract_id_reads_named_key() {
        assert_eq!(
            extract_id(r#"{"squad_id":"squad-1"}"#, "squad_id").as_deref(),
            Some("squad-1")
        );
        assert_eq!(extract_id(r#"{"run_id":"r"}"#, "squad_id"), None);
    }

    #[test]
    fn rng_below_stays_in_range() {
        let mut r = Rng::new(1);
        for _ in 0..1000 {
            assert!(r.below(7) < 7);
        }
        assert_eq!(r.below(0), 0);
    }
}
