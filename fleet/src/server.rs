//! The HTTP side of the service: the allocation API ralphus calls with the
//! service token, each allocation's Machine API it calls with that
//! allocation's token (`docs/machine-provider-service.md`), and the outpost
//! channel (`/v1/outpost/*`) outposts call with their sealed session
//! (`docs/outpost.md`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::allocations::{Allocations, Platform, Requirements, choose, constant_time_eq};
use crate::config::{FleetConfig, MachineConfig};
use crate::machine_api::{self, PROTOCOL_VERSION, Reach, refusal};
use crate::outposts::Outposts;

/// One running service.
pub struct Service {
    config: FleetConfig,
    /// Base URL allocations' endpoints are under.
    base_url: String,
    token: String,
    allocations: Mutex<Allocations>,
    /// Probed os/arch per machine name; configured values win.
    platforms: Mutex<BTreeMap<String, Platform>>,
    outposts: Outposts,
}

/// A reply: status and JSON body.
pub type Reply = (u16, Value);

fn log(message: &str) {
    eprintln!("ralphus-fleet: {message}");
}

impl Service {
    /// A service whose outposts' pairing secrets come from this process's
    /// environment.
    #[must_use]
    pub fn new(
        config: FleetConfig,
        base_url: String,
        token: String,
        allocations: Allocations,
    ) -> Self {
        Self::with_outpost_secrets(config, base_url, token, allocations, |variable| {
            std::env::var(variable).ok()
        })
    }

    /// A service whose outposts' pairing secrets come from `secret_of`
    /// (given each machine's `outpost_token_env`).
    #[must_use]
    pub fn with_outpost_secrets(
        config: FleetConfig,
        base_url: String,
        token: String,
        allocations: Allocations,
        secret_of: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let outposts = Outposts::new(&config, secret_of);
        let platforms = config
            .machines
            .iter()
            .map(|m| {
                (
                    m.display_name().to_string(),
                    Platform {
                        os: m.os.clone(),
                        arch: m.arch.clone(),
                    },
                )
            })
            .collect();
        Self {
            config,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            allocations: Mutex::new(allocations),
            platforms: Mutex::new(platforms),
            outposts,
        }
    }

    fn reach(&self) -> Reach<'_> {
        Reach {
            config: &self.config,
            outposts: &self.outposts,
        }
    }

    fn platform(&self, name: &str) -> Platform {
        self.platforms
            .lock()
            .expect("poisoned")
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    fn machine(&self, name: &str) -> Option<&MachineConfig> {
        self.config
            .machines
            .iter()
            .find(|m| m.display_name() == name)
    }

    /// Ask each machine whose os or arch is not configured, once.
    fn probe_platforms(&self) {
        let unknown: Vec<&MachineConfig> = {
            let platforms = self.platforms.lock().expect("poisoned");
            self.config
                .machines
                .iter()
                .filter(|m| {
                    platforms
                        .get(m.display_name())
                        .is_none_or(|p| p.os.is_none() || p.arch.is_none())
                })
                .collect()
        };
        for machine in unknown {
            if machine.is_outpost() && !self.outposts.connected(machine.display_name()) {
                // Learned when it connects; asking now would only fail.
                continue;
            }
            let platform = self.platform(machine.display_name());
            let reply =
                machine_api::serve(&self.reach(), machine, &platform, "capabilities", json!({}));
            let caps = &reply["capabilities"];
            let mut platforms = self.platforms.lock().expect("poisoned");
            let platform = platforms
                .entry(machine.display_name().to_string())
                .or_default();
            if platform.os.is_none() {
                platform.os = caps["os"].as_str().map(str::to_lowercase);
            }
            if platform.arch.is_none() {
                platform.arch = caps["arch"].as_str().map(str::to_lowercase);
            }
            if reply["ok"] != true {
                log(&format!(
                    "could not ask {} for its platform: {}",
                    machine.display_name(),
                    reply["error"]
                ));
            }
        }
    }

    fn allocation_json(
        &self,
        id: &str,
        machine: &MachineConfig,
        token: Option<(String, i64)>,
    ) -> Value {
        let platform = self
            .platforms
            .lock()
            .expect("poisoned")
            .get(machine.display_name())
            .cloned()
            .unwrap_or_default();
        let mut reply = json!({
            "protocol_version": PROTOCOL_VERSION,
            "allocation_id": id,
            "endpoint": format!("{}/v1/m/{id}", self.base_url),
            "os": platform.os,
            "arch": platform.arch,
            "labels": machine.labels,
            "description": machine.display_name(),
        });
        if let Some((token, expires)) = token {
            reply["token"] = token.into();
            reply["token_expires_at_ms"] = expires.into();
        }
        reply
    }

    fn allocate(&self, body: &Value) -> Reply {
        let wanted: Requirements =
            serde_json::from_value(body["requirements"].clone()).unwrap_or_default();
        let scope = body["scope"].clone();
        // A repeated request for the same scope gets the same machine.
        let existing = self
            .allocations
            .lock()
            .expect("poisoned")
            .for_scope(&scope)
            .cloned();
        if let Some(existing) = existing {
            if let Some(machine) = self.machine(&existing.machine) {
                return self.renewed(&existing.id, machine);
            }
        }
        self.probe_platforms();
        let platforms = self.platforms.lock().expect("poisoned").clone();
        let mut allocations = self.allocations.lock().expect("poisoned");
        let machine = match choose(&self.config, &platforms, &allocations, &wanted) {
            Ok(machine) => machine,
            Err(r) => return (409, refusal(r.reason_code, &r.error, &r.remediation)),
        };
        let allocation = match allocations.insert(machine.display_name(), scope) {
            Ok(a) => a,
            Err(e) => {
                return (
                    500,
                    refusal("unavailable", &e, "check the fleet's state_file"),
                );
            }
        };
        let token = match allocations.issue_token(&allocation.id) {
            Ok(t) => t,
            Err(e) => return (500, refusal("unavailable", &e, "check the fleet host")),
        };
        drop(allocations);
        log(&format!(
            "allocated {} on {}",
            allocation.id,
            machine.display_name()
        ));
        (
            200,
            self.allocation_json(&allocation.id, machine, Some(token)),
        )
    }

    fn renewed(&self, id: &str, machine: &MachineConfig) -> Reply {
        match self.allocations.lock().expect("poisoned").issue_token(id) {
            Ok(token) => (200, self.allocation_json(id, machine, Some(token))),
            Err(e) => (500, refusal("unavailable", &e, "check the fleet host")),
        }
    }

    fn unknown(id: &str) -> Reply {
        (
            404,
            refusal(
                "unknown_allocation",
                &format!("no allocation {id}"),
                "ask for a new machine",
            ),
        )
    }

    /// Answer one request. `auth` is the bearer token presented, if any.
    #[must_use]
    pub fn handle(&self, method: &str, path: &str, auth: Option<&str>, body: &Value) -> Reply {
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        if let ["v1", "m", id, verb] = segments.as_slice() {
            return self.machine_api(id, verb, auth, body.clone());
        }
        if let ("POST", ["v1", "outpost", op]) = (method, segments.as_slice()) {
            return self.outpost(op, body);
        }
        if !auth.is_some_and(|t| constant_time_eq(t.as_bytes(), self.token.as_bytes())) {
            return (
                401,
                refusal(
                    "unauthorized",
                    "the service token was refused",
                    "set the daemon's token_env variable to the fleet's token",
                ),
            );
        }
        match (method, segments.as_slice()) {
            ("GET", ["v1", "health"]) => (
                200,
                json!({"protocol_version": PROTOCOL_VERSION, "version": env!("CARGO_PKG_VERSION")}),
            ),
            ("POST", ["v1", "allocations"]) => self.allocate(body),
            ("GET", ["v1", "outposts"]) => (
                200,
                json!({"protocol_version": PROTOCOL_VERSION, "outposts": self.outposts.list()}),
            ),
            ("GET", ["v1", "allocations", id]) => {
                let found = self.allocations.lock().expect("poisoned").get(id).cloned();
                match found.and_then(|a| self.machine(&a.machine).map(|m| (a, m))) {
                    Some((a, machine)) => (200, self.allocation_json(&a.id, machine, None)),
                    None => Self::unknown(id),
                }
            }
            ("POST", ["v1", "allocations", id, "renew"]) => {
                let found = self.allocations.lock().expect("poisoned").get(id).cloned();
                match found.and_then(|a| self.machine(&a.machine)) {
                    Some(machine) => self.renewed(id, machine),
                    None => Self::unknown(id),
                }
            }
            ("POST", ["v1", "allocations", id, "idle"]) => {
                let idle = body["idle"].as_bool().unwrap_or(false);
                match self
                    .allocations
                    .lock()
                    .expect("poisoned")
                    .set_idle(id, idle)
                {
                    Ok(true) => (200, json!({"protocol_version": PROTOCOL_VERSION})),
                    Ok(false) => Self::unknown(id),
                    Err(e) => (
                        500,
                        refusal("unavailable", &e, "check the fleet's state_file"),
                    ),
                }
            }
            ("DELETE", ["v1", "allocations", id]) => {
                match self.allocations.lock().expect("poisoned").remove(id) {
                    Ok(true) => {
                        log(&format!("released {id}"));
                        (200, json!({"protocol_version": PROTOCOL_VERSION}))
                    }
                    Ok(false) => Self::unknown(id),
                    Err(e) => (
                        500,
                        refusal("unavailable", &e, "check the fleet's state_file"),
                    ),
                }
            }
            _ => (
                404,
                refusal(
                    "unsupported",
                    &format!("no route {method} {path}"),
                    "see docs/machine-provider-service.md",
                ),
            ),
        }
    }

    fn machine_api(&self, id: &str, verb: &str, auth: Option<&str>, body: Value) -> Reply {
        let (allocation, valid) = {
            let allocations = self.allocations.lock().expect("poisoned");
            let allocation = allocations.get(id).cloned();
            let valid = auth.is_some_and(|t| allocations.token_valid(id, t));
            (allocation, valid)
        };
        let Some(allocation) = allocation else {
            return Self::unknown(id);
        };
        if !valid {
            return (
                401,
                refusal(
                    "unauthorized",
                    "the allocation token was refused",
                    "renew the allocation",
                ),
            );
        }
        let Some(machine) = self.machine(&allocation.machine) else {
            return (
                410,
                refusal(
                    "unknown_allocation",
                    &format!("machine {} was removed from the fleet", allocation.machine),
                    "ask for a new machine",
                ),
            );
        };
        let platform = self.platform(machine.display_name());
        (
            200,
            machine_api::serve(&self.reach(), machine, &platform, verb, body),
        )
    }

    /// The outpost channel: `hello`, `poll` and `reply`.
    fn outpost(&self, op: &str, body: &Value) -> Reply {
        let result = match op {
            "hello" => self.outposts.hello(body),
            "poll" => {
                let result = self.outposts.poll(body);
                if result.is_ok() {
                    self.learn_outpost_platforms();
                }
                result
            }
            "reply" => self.outposts.reply(body),
            _ => Err((
                404,
                refusal(
                    "unsupported",
                    &format!("no outpost operation {op:?}"),
                    "see docs/outpost.md",
                ),
            )),
        };
        match result {
            Ok(reply) => (200, reply),
            Err((status, reply)) => (status, reply),
        }
    }

    /// Record the os/arch connected outposts reported, where the
    /// configuration does not say.
    fn learn_outpost_platforms(&self) {
        let mut platforms = self.platforms.lock().expect("poisoned");
        for machine in self.config.machines.iter().filter(|m| m.is_outpost()) {
            let info = self.outposts.info(machine.display_name());
            let platform = platforms
                .entry(machine.display_name().to_string())
                .or_default();
            if platform.os.is_none() {
                platform.os = info["os"].as_str().map(str::to_string);
            }
            if platform.arch.is_none() {
                platform.arch = info["arch"].as_str().map(str::to_string);
            }
        }
    }
}

/// Serve on `config.listen` until the process ends. Each request is answered
/// on its own thread, since an `exec` may take as long as a cell.
///
/// # Errors
/// The listener could not be opened.
pub fn run(service: Service) -> Result<(), String> {
    let server = tiny_http::Server::http(&service.config.listen)
        .map_err(|e| format!("could not listen on {}: {e}", service.config.listen))?;
    serve_on(&server, service);
    Ok(())
}

/// Serve requests arriving at `server` until it is unblocked or dropped.
pub fn serve_on(server: &tiny_http::Server, service: Service) {
    log(&format!(
        "serving {} machine(s) on {}",
        service.config.machines.len(),
        server.server_addr()
    ));
    let service = Arc::new(service);
    for mut request in server.incoming_requests() {
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            let auth = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .and_then(|h| h.value.as_str().strip_prefix("Bearer ").map(str::to_string));
            let mut text = String::new();
            let _ = request.as_reader().read_to_string(&mut text);
            let body = serde_json::from_str(&text).unwrap_or(Value::Null);
            let method = request.method().to_string();
            let path = request
                .url()
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();
            let (status, reply) = service.handle(&method, &path, auth.as_deref(), &body);
            let header = tiny_http::Header::from_bytes("Content-Type", "application/json")
                .expect("static header");
            let _ = request.respond(
                tiny_http::Response::from_string(reply.to_string())
                    .with_status_code(status)
                    .with_header(header),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> Service {
        let config = FleetConfig::parse(
            "token_env = \"T\"\n\
             [[machine]]\nhost = \"lin\"\nos = \"linux\"\narch = \"x86_64\"\nremote_root = \"/r\"\n",
            "f",
        )
        .unwrap();
        Service::new(
            config,
            "http://fleet:7900".to_string(),
            "svc".to_string(),
            Allocations::default(),
        )
    }

    #[test]
    fn the_allocation_api_needs_the_service_token() {
        let s = service();
        let (status, reply) = s.handle("POST", "/v1/allocations", Some("wrong"), &json!({}));
        assert_eq!(status, 401);
        assert_eq!(reply["reason_code"], "unauthorized");
    }

    #[test]
    fn an_allocation_is_granted_reused_for_its_scope_and_released() {
        let s = service();
        let request = json!({
            "requirements": {"os": "linux"},
            "scope": {"squad_id": "s1", "key": "@fleet?os=linux"},
        });
        let (status, granted) = s.handle("POST", "/v1/allocations", Some("svc"), &request);
        assert_eq!(status, 200, "{granted}");
        let id = granted["allocation_id"].as_str().unwrap().to_string();
        assert_eq!(granted["endpoint"], format!("http://fleet:7900/v1/m/{id}"));
        assert_eq!(granted["os"], "linux");
        assert!(granted["token"].as_str().is_some_and(|t| t.len() == 64));

        let (_, again) = s.handle("POST", "/v1/allocations", Some("svc"), &request);
        assert_eq!(
            again["allocation_id"],
            id.as_str(),
            "same scope, same machine"
        );

        let (status, read) = s.handle(
            "GET",
            &format!("/v1/allocations/{id}"),
            Some("svc"),
            &Value::Null,
        );
        assert_eq!(status, 200);
        assert!(read.get("token").is_none(), "a read hands out no token");

        let (status, _) = s.handle(
            "DELETE",
            &format!("/v1/allocations/{id}"),
            Some("svc"),
            &Value::Null,
        );
        assert_eq!(status, 200);
        let (status, gone) = s.handle(
            "GET",
            &format!("/v1/allocations/{id}"),
            Some("svc"),
            &Value::Null,
        );
        assert_eq!(status, 404);
        assert_eq!(gone["reason_code"], "unknown_allocation");
    }

    #[test]
    fn the_machine_api_needs_the_allocation_token_not_the_service_token() {
        let s = service();
        let request = json!({"requirements": {}, "scope": {"squad_id": "s2", "key": "k"}});
        let (_, granted) = s.handle("POST", "/v1/allocations", Some("svc"), &request);
        let id = granted["allocation_id"].as_str().unwrap();
        let (status, reply) =
            s.handle("POST", &format!("/v1/m/{id}/ping"), Some("svc"), &json!({}));
        assert_eq!(status, 401);
        assert_eq!(reply["reason_code"], "unauthorized");
        let (status, _) = s.handle("POST", "/v1/m/alloc-nope/ping", Some("svc"), &json!({}));
        assert_eq!(status, 404);
    }

    #[test]
    fn a_request_no_machine_fits_is_refused_with_the_reason() {
        let s = service();
        let request =
            json!({"requirements": {"os": "windows"}, "scope": {"squad_id": "s3", "key": "k"}});
        let (status, reply) = s.handle("POST", "/v1/allocations", Some("svc"), &request);
        assert_eq!(status, 409);
        assert_eq!(reply["reason_code"], "no_matching_machine");
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .contains("lin runs linux, not windows")
        );
    }
}
