//! The fleet side of the outpost transport: machines that connect *out* to
//! this service with `ralphus-outpost`, poll it for Machine API requests,
//! run them and post the replies back (`docs/outpost.md`).
//!
//! [`Outposts::call`] queues one request for a machine and waits for its
//! reply; [`Outposts::hello`], [`Outposts::poll`] and [`Outposts::reply`]
//! serve the outpost's side of the conversation, authenticated and sealed
//! as [`crate::outpost_wire`] (a pairing secret from `outpost_token_env`)
//! or [`crate::outpost_keys`] (a machine paired with `ralphus-fleet pair`)
//! describes. [`Outposts::pair`] completes a pairing, and
//! [`Outposts::runner`] hands an outpost the runner it should run jobs with.
//! Each poll answer tells the outpost what the fleet expects of it (the
//! agents to check, the runner's hash), and each poll may carry the
//! machine's readiness report back.

use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::config::FleetConfig;
use crate::outpost_keys::{self, Ephemeral, Identity, PAIR_PROTOCOL, PROTOCOL_V2};
use crate::outpost_registry::{Registry, RegistryFile};
use crate::outpost_wire::{self, FLEET_TO_OUTPOST, OUTPOST_TO_FLEET, SessionKey};
use crate::readiness::Report;
use crate::runner_artifacts::RunnerArtifacts;

/// How long a poll waits for work before answering with none.
pub const POLL_WAIT: Duration = Duration::from_secs(20);
/// A session not heard from for this long is gone.
const STALE: Duration = Duration::from_secs(75);
/// How long a hello waits for the outpost's first sealed poll.
const PENDING_TTL: Duration = Duration::from_secs(60);
/// Unconfirmed hellos kept per machine.
const MAX_PENDING: usize = 8;

/// What a machine's outpost answered.
#[derive(Debug, Clone)]
pub struct Answer {
    pub reply: Value,
    pub events: Vec<String>,
}

type Waiter = mpsc::Sender<Result<Answer, String>>;

struct Outgoing {
    id: u64,
    verb: String,
    body: Value,
    context: Value,
    waiter: Waiter,
}

struct Session {
    id: String,
    key: Arc<SessionKey>,
    last_seen: Instant,
    last_seq: u64,
    in_flight: BTreeMap<u64, Waiter>,
    /// The paired public key this session was authenticated with (`v2`).
    machine_key: Option<String>,
}

/// What the outpost must sign in its first poll for a `v2` session.
struct ExpectedProof {
    machine_key: String,
    transcript: String,
}

struct Pending {
    name: String,
    session: Session,
    created: Instant,
    proof: Option<ExpectedProof>,
}

#[derive(Default)]
struct Slot {
    active: Option<Session>,
    queue: VecDeque<Outgoing>,
    info: Value,
    readiness: Option<Report>,
}

#[derive(Default)]
struct State {
    slots: BTreeMap<String, Slot>,
    pending: Vec<Pending>,
    next_id: u64,
}

/// The registry as last read, re-read when the file changes.
#[derive(Default)]
struct RegistryCache {
    modified: Option<SystemTime>,
    loaded: bool,
    file: RegistryFile,
}

/// A refusal to an outpost: HTTP status and body.
pub type Refusal = (u16, Value);

fn refused(status: u16, error: &str) -> Refusal {
    (
        status,
        json!({"ok": false, "reason_code": "unauthorized", "error": error}),
    )
}

/// Every outpost machine's connection state.
pub struct Outposts {
    secrets: BTreeMap<String, String>,
    /// Machine name → `remote_root`, for pairing answers.
    remote_roots: BTreeMap<String, String>,
    /// Machine name → the agents its outpost should check (`name` → path or
    /// null for "as found on the machine").
    agents: BTreeMap<String, Value>,
    identity: Option<Identity>,
    registry: Option<Registry>,
    registry_cache: Mutex<RegistryCache>,
    runners: RunnerArtifacts,
    state: Mutex<State>,
    wake: Condvar,
}

impl Outposts {
    /// Outposts for `config`'s outpost machines, with their pairing secrets
    /// read from `secret_of(<outpost_token_env>)`. A machine with neither a
    /// usable secret nor (once [`Self::with_pairing`] is given) a paired key
    /// refuses every connection.
    #[must_use]
    pub fn new(config: &FleetConfig, secret_of: impl Fn(&str) -> Option<String>) -> Self {
        let mut secrets = BTreeMap::new();
        let mut slots = BTreeMap::new();
        let mut remote_roots = BTreeMap::new();
        let mut agents = BTreeMap::new();
        for machine in config.machines.iter().filter(|m| m.is_outpost()) {
            let name = machine.display_name().to_string();
            if let Some(variable) = machine.outpost_token_env.clone() {
                match secret_of(&variable).map(|s| s.trim().to_string()) {
                    Some(secret) if outpost_wire::check_secret(&secret).is_ok() => {
                        secrets.insert(name.clone(), secret);
                    }
                    Some(_) => eprintln!(
                        "ralphus-fleet: outpost machine {name:?}: {variable} is shorter than {} \
                         characters; it will refuse secret-based connections",
                        outpost_wire::MIN_SECRET_LEN
                    ),
                    None => eprintln!(
                        "ralphus-fleet: outpost machine {name:?}: {variable} is not set; it will \
                         refuse secret-based connections"
                    ),
                }
            }
            // Every CLI agent is checked, so a request naming one can be
            // judged; configured paths and required agents are added by
            // their backend name.
            let mut wanted = serde_json::Map::new();
            for agent in crate::readiness::CLI_AGENTS {
                wanted.insert(agent.to_string(), Value::Null);
            }
            for agent in &machine.required_agents {
                let agent = crate::readiness::canonical_agent(agent).unwrap_or(agent);
                wanted.entry(agent.to_string()).or_insert(Value::Null);
            }
            for (agent, path) in &machine.agents {
                let agent = crate::readiness::canonical_agent(agent).unwrap_or(agent);
                wanted.insert(agent.to_string(), path.clone().into());
            }

            agents.insert(name.clone(), Value::Object(wanted));
            remote_roots.insert(name.clone(), machine.remote_root.clone());
            slots.insert(name, Slot::default());
        }
        Self {
            secrets,
            remote_roots,
            agents,
            identity: None,
            registry: None,
            registry_cache: Mutex::new(RegistryCache::default()),
            runners: RunnerArtifacts::new(config.outpost_runners.clone()),
            state: Mutex::new(State {
                slots,
                ..State::default()
            }),
            wake: Condvar::new(),
        }
    }

    /// Keep readiness reports (and paired keys) in `registry`.
    #[must_use]
    pub fn with_registry(mut self, registry: Registry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Accept paired outposts and pairings, proving this fleet is the one
    /// they paired with by `identity`.
    #[must_use]
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The registry as it is on disk now (re-read only when it changed).
    fn registry_now(&self) -> RegistryFile {
        let Some(registry) = &self.registry else {
            return RegistryFile::default();
        };
        let mut cache = self
            .registry_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let modified = registry.modified();
        if !cache.loaded || cache.modified != modified {
            match registry.load() {
                Ok(file) => {
                    cache.file = file;
                    cache.modified = modified;
                    cache.loaded = true;
                }
                Err(e) => eprintln!("ralphus-fleet: {e}"),
            }
        }
        cache.file.clone()
    }

    /// The public key `name` paired with, if it has one now.
    fn paired_key(&self, name: &str) -> Option<String> {
        self.registry_now()
            .machines
            .get(name)
            .and_then(|e| e.public_key.clone())
    }

    /// Whether `name`'s outpost is connected now.
    #[must_use]
    pub fn connected(&self, name: &str) -> bool {
        let mut state = self.lock();
        state
            .slots
            .get_mut(name)
            .is_some_and(|slot| expire(slot).is_some())
    }

    /// What `name`'s outpost said about itself when it connected.
    #[must_use]
    pub fn info(&self, name: &str) -> Value {
        self.lock()
            .slots
            .get(name)
            .map(|s| s.info.clone())
            .unwrap_or(Value::Null)
    }

    /// `name`'s latest readiness report: from its live session, else the
    /// last one recorded.
    #[must_use]
    pub fn readiness(&self, name: &str) -> Option<Report> {
        let live = self
            .lock()
            .slots
            .get(name)
            .and_then(|s| s.readiness.clone());
        live.or_else(|| {
            self.registry_now()
                .machines
                .get(name)
                .and_then(|e| e.readiness.clone())
        })
    }

    /// Every outpost machine: whether it is connected, how it authenticates,
    /// and its readiness.
    #[must_use]
    pub fn list(&self) -> Vec<Value> {
        let registry = self.registry_now();
        let mut state = self.lock();
        state
            .slots
            .iter_mut()
            .map(|(name, slot)| {
                let connected = expire(slot).is_some();
                let entry = registry.machines.get(name);
                let key = entry.and_then(|e| e.public_key.as_deref());
                json!({
                    "name": name,
                    "connected": connected,
                    "info": slot.info,
                    "paired": key.is_some(),
                    "fingerprint": key.map(outpost_keys::fingerprint),
                    "secret": self.secrets.contains_key(name),
                    "readiness": slot.readiness.clone().or_else(|| entry.and_then(|e| e.readiness.clone())),
                })
            })
            .collect()
    }

    /// Send `verb` to `name`'s outpost and wait for its answer.
    ///
    /// # Errors
    /// The outpost is not connected, or its connection was lost or replaced
    /// before it answered.
    pub fn call(
        &self,
        name: &str,
        verb: &str,
        body: Value,
        context: Value,
    ) -> Result<Answer, String> {
        let (tx, rx) = mpsc::channel();
        let id = {
            let mut state = self.lock();
            state.next_id += 1;
            let id = state.next_id;
            let slot = state
                .slots
                .get_mut(name)
                .ok_or_else(|| format!("{name:?} is not an outpost machine"))?;
            if expire(slot).is_none() {
                return Err(not_connected(name));
            }
            slot.queue.push_back(Outgoing {
                id,
                verb: verb.to_string(),
                body,
                context,
                waiter: tx,
            });
            id
        };
        self.wake.notify_all();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(answer) => return answer,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(format!("the request to outpost {name:?} was dropped"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let mut state = self.lock();
                    let Some(slot) = state.slots.get_mut(name) else {
                        return Err(not_connected(name));
                    };
                    if expire(slot).is_none() {
                        slot.queue.retain(|o| o.id != id);
                        return Err(format!(
                            "outpost {name:?} stopped answering before it replied to {verb}"
                        ));
                    }
                }
            }
        }
    }

    /// An outpost's hello: the session it may confirm, the fleet's nonce and
    /// its proof (`v1`: an HMAC under the pairing secret; `v2`: a signature
    /// by this fleet's identity, plus the fleet's ephemeral key).
    ///
    /// # Errors
    /// An unknown machine, a machine that cannot authenticate this way, or
    /// a malformed hello -- all answered alike, so a stranger learns
    /// nothing.
    pub fn hello(&self, body: &Value) -> Result<Value, Refusal> {
        let refusal = || refused(401, "the outpost was refused");
        let name = body["name"].as_str().unwrap_or_default();
        let outpost_nonce = body["nonce"].as_str().unwrap_or_default();
        if !outpost_wire::is_hex(outpost_nonce, 32) {
            return Err(refusal());
        }
        let random = |n| outpost_wire::random_hex(n).map_err(|e| refused(500, &e));
        let session = random(16)?;
        let fleet_nonce = random(32)?;
        match body["protocol"].as_str() {
            Some(outpost_wire::PROTOCOL) => {
                let Some(secret) = self.secrets.get(name) else {
                    eprintln!("ralphus-fleet: refused an outpost hello for {name:?}");
                    return Err(refusal());
                };
                let key = SessionKey::derive(secret, name, outpost_nonce, &fleet_nonce, &session)
                    .map_err(|e| refused(500, &e))?;
                let proof =
                    outpost_wire::fleet_proof(secret, name, outpost_nonce, &fleet_nonce, &session);
                self.add_pending(name, &session, key, None);
                Ok(json!({"session": session, "nonce": fleet_nonce, "proof": proof}))
            }
            Some(PROTOCOL_V2) => {
                let outpost_ephemeral = body["ephemeral"].as_str().unwrap_or_default();
                let (Some(identity), Some(machine_key)) = (&self.identity, self.paired_key(name))
                else {
                    eprintln!("ralphus-fleet: refused a paired outpost hello for {name:?}");
                    return Err(refusal());
                };
                if !self.remote_roots.contains_key(name) {
                    return Err(refusal());
                }
                let ephemeral = Ephemeral::generate().map_err(|e| refused(500, &e))?;
                let fleet_ephemeral = ephemeral.public().to_string();
                let shared = ephemeral.agree(outpost_ephemeral).map_err(|_| refusal())?;
                let key =
                    SessionKey::derive_v2(&shared, name, outpost_nonce, &fleet_nonce, &session)
                        .map_err(|e| refused(500, &e))?;
                let transcript = |role| {
                    outpost_keys::transcript_v2(
                        role,
                        name,
                        outpost_nonce,
                        outpost_ephemeral,
                        &fleet_nonce,
                        &fleet_ephemeral,
                        &session,
                    )
                };
                let signature = identity.sign(&transcript("fleet"));
                self.add_pending(
                    name,
                    &session,
                    key,
                    Some(ExpectedProof {
                        machine_key,
                        transcript: transcript("outpost"),
                    }),
                );
                Ok(json!({
                    "session": session,
                    "nonce": fleet_nonce,
                    "ephemeral": fleet_ephemeral,
                    "signature": signature,
                }))
            }
            _ => Err(refusal()),
        }
    }

    fn add_pending(
        &self,
        name: &str,
        session: &str,
        key: SessionKey,
        proof: Option<ExpectedProof>,
    ) {
        let mut state = self.lock();
        state.pending.retain(|p| p.created.elapsed() < PENDING_TTL);
        while state.pending.iter().filter(|p| p.name == name).count() >= MAX_PENDING {
            if let Some(oldest) = state.pending.iter().position(|p| p.name == name) {
                state.pending.remove(oldest);
            }
        }
        let machine_key = proof.as_ref().map(|p| p.machine_key.clone());
        state.pending.push(Pending {
            name: name.to_string(),
            session: Session {
                id: session.to_string(),
                key: Arc::new(key),
                last_seen: Instant::now(),
                last_seq: 0,
                in_flight: BTreeMap::new(),
                machine_key,
            },
            created: Instant::now(),
            proof,
        });
    }

    /// Find the session `id`, promoting a pending one whose first sealed
    /// message opened (and, for `v2`, whose `proof` is the machine's
    /// signature). Returns the machine name and the opened message.
    fn open(
        &self,
        state: &mut State,
        id: &str,
        sealed: &str,
        proof: Option<&str>,
    ) -> Result<(String, Value), Refusal> {
        let unknown = || refused(401, "unknown or expired outpost session");
        let open = |key: &SessionKey| -> Result<Value, Refusal> {
            let plain = key
                .open(OUTPOST_TO_FLEET, sealed)
                .map_err(|_| refused(401, "the outpost message did not authenticate"))?;
            serde_json::from_slice(&plain).map_err(|_| refused(400, "malformed outpost message"))
        };
        for (name, slot) in &mut state.slots {
            if let Some(active) = slot.active.as_mut().filter(|a| a.id == id) {
                let message = open(&active.key)?;
                active.last_seen = Instant::now();
                return Ok((name.clone(), message));
            }
        }
        let at = state
            .pending
            .iter()
            .position(|p| p.session.id == id && p.created.elapsed() < PENDING_TTL)
            .ok_or_else(unknown)?;
        let message = open(&state.pending[at].session.key)?;
        if let Some(expected) = &state.pending[at].proof {
            let signed = proof.is_some_and(|p| {
                outpost_keys::verify_signature(&expected.machine_key, &expected.transcript, p)
            });
            if !signed {
                state.pending.remove(at);
                return Err(refused(
                    401,
                    "the outpost could not prove it holds this machine's paired credential",
                ));
            }
        }
        let pending = state.pending.remove(at);
        let slot = state.slots.entry(pending.name.clone()).or_default();
        if let Some(old) = slot.active.take() {
            for (_, waiter) in old.in_flight {
                let _ = waiter.send(Err(format!(
                    "outpost {:?} reconnected before it answered; the request was lost",
                    pending.name
                )));
            }
        }
        eprintln!("ralphus-fleet: outpost {} connected", pending.name);
        slot.active = Some(pending.session);
        Ok((pending.name, message))
    }

    /// End `name`'s session `session` (it was revoked or re-paired), failing
    /// its unanswered requests.
    fn drop_session(state: &mut State, name: &str, session: &str, why: &str) {
        if let Some(slot) = state.slots.get_mut(name) {
            if slot.active.as_ref().is_some_and(|a| a.id == session) {
                if let Some(old) = slot.active.take() {
                    for (_, waiter) in old.in_flight {
                        let _ = waiter.send(Err(why.to_string()));
                    }
                }
            }
        }
    }

    /// What the fleet expects of `name`'s outpost: the agents to check and
    /// the runner to run jobs with.
    fn expectation(&self, name: &str, info: &Value) -> Value {
        let runner = match (info["os"].as_str(), info["arch"].as_str()) {
            (Some(os), Some(arch)) => self.runners.expected(os, arch),
            _ => None,
        };
        json!({
            "agents": self.agents.get(name).cloned().unwrap_or_else(|| json!({})),
            "runner": runner,
        })
    }

    /// A sealed poll: wait up to [`POLL_WAIT`] for requests and hand them
    /// over, sealed, with what the fleet expects of the outpost.
    ///
    /// # Errors
    /// An unknown session, a message that does not authenticate, a
    /// replayed sequence number, or a machine whose pairing was revoked.
    pub fn poll(&self, body: &Value) -> Result<Value, Refusal> {
        let session = body["session"].as_str().unwrap_or_default().to_string();
        let sealed = body["box"].as_str().unwrap_or_default();
        let (name, seq, machine_key, readiness, info) = {
            let mut state = self.lock();
            let (name, message) =
                self.open(&mut state, &session, sealed, body["proof"].as_str())?;
            let seq = message["seq"].as_u64().unwrap_or(0);
            let slot = state
                .slots
                .get_mut(&name)
                .ok_or_else(|| refused(401, "gone"))?;
            let active = slot.active.as_mut().ok_or_else(|| refused(401, "gone"))?;
            if seq <= active.last_seq {
                return Err(refused(401, "a replayed outpost poll"));
            }
            active.last_seq = seq;
            let machine_key = active.machine_key.clone();
            if message["info"].is_object() {
                slot.info = message["info"].clone();
            }
            let readiness = serde_json::from_value::<Report>(message["readiness"].clone()).ok();
            if let Some(report) = &readiness {
                slot.readiness = Some(report.clone());
            }
            (name, seq, machine_key, readiness, slot.info.clone())
        };
        if let Some(key) = machine_key {
            if self.paired_key(&name).as_deref() != Some(key.as_str()) {
                let why = format!("outpost {name:?}'s pairing was revoked");
                Self::drop_session(&mut self.lock(), &name, &session, &why);
                eprintln!("ralphus-fleet: {why}; its session ended");
                return Err(refused(
                    401,
                    "this machine's pairing was revoked; pair it again with `ralphus-fleet pair`",
                ));
            }
        }
        if let (Some(report), Some(registry)) = (&readiness, &self.registry) {
            if let Err(e) = registry.set_readiness(&name, report) {
                eprintln!("ralphus-fleet: could not record {name}'s readiness: {e}");
            }
        }
        let expect = self.expectation(&name, &info);
        let deadline = Instant::now() + POLL_WAIT;
        let mut state = self.lock();
        loop {
            let slot = state
                .slots
                .get_mut(&name)
                .ok_or_else(|| refused(401, "gone"))?;
            let still_active = slot.active.as_ref().is_some_and(|a| a.id == session);
            if !still_active {
                return Err(refused(401, "the outpost session was replaced"));
            }
            if !slot.queue.is_empty() || Instant::now() >= deadline {
                break;
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            state = self
                .wake
                .wait_timeout(state, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        let slot = state
            .slots
            .get_mut(&name)
            .ok_or_else(|| refused(401, "gone"))?;
        let active = slot.active.as_mut().ok_or_else(|| refused(401, "gone"))?;
        active.last_seen = Instant::now();
        let mut requests = Vec::new();
        while let Some(outgoing) = slot.queue.pop_front() {
            requests.push(json!({
                "id": outgoing.id,
                "verb": outgoing.verb,
                "body": outgoing.body,
                "context": outgoing.context,
            }));
            active.in_flight.insert(outgoing.id, outgoing.waiter);
        }
        let plain = json!({"seq": seq, "requests": requests, "expect": expect}).to_string();
        let sealed = active
            .key
            .seal(FLEET_TO_OUTPOST, plain.as_bytes())
            .map_err(|e| refused(500, &e))?;
        Ok(json!({"box": sealed}))
    }

    /// A sealed reply to one request.
    ///
    /// # Errors
    /// An unknown session or a message that does not authenticate.
    pub fn reply(&self, body: &Value) -> Result<Value, Refusal> {
        let session = body["session"].as_str().unwrap_or_default().to_string();
        let sealed = body["box"].as_str().unwrap_or_default();
        let mut state = self.lock();
        let (name, message) = self.open(&mut state, &session, sealed, None)?;
        let id = message["id"].as_u64().unwrap_or(0);
        let waiter = state
            .slots
            .get_mut(&name)
            .and_then(|slot| slot.active.as_mut())
            .and_then(|active| active.in_flight.remove(&id));
        if let Some(waiter) = waiter {
            let events = message["events"]
                .as_array()
                .map(|events| {
                    events
                        .iter()
                        .filter_map(|e| e.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let _ = waiter.send(Ok(Answer {
                reply: message["reply"].clone(),
                events,
            }));
        }
        Ok(json!({}))
    }

    /// A sealed request for the runner the last poll answer named
    /// (`{"sha256"}`): the binary, sealed.
    ///
    /// # Errors
    /// An unknown session, a message that does not authenticate, or a
    /// runner that is not configured or changed since.
    pub fn runner(&self, body: &Value) -> Result<Value, Refusal> {
        let session = body["session"].as_str().unwrap_or_default().to_string();
        let sealed = body["box"].as_str().unwrap_or_default();
        let (name, message, key, info) = {
            let mut state = self.lock();
            let (name, message) = self.open(&mut state, &session, sealed, None)?;
            let slot = state.slots.get(&name).ok_or_else(|| refused(401, "gone"))?;
            let active = slot.active.as_ref().ok_or_else(|| refused(401, "gone"))?;
            (name, message, Arc::clone(&active.key), slot.info.clone())
        };
        let sha256 = message["sha256"].as_str().unwrap_or_default();
        let (Some(os), Some(arch)) = (info["os"].as_str(), info["arch"].as_str()) else {
            return Err((
                409,
                json!({"ok": false, "error": "the outpost has not said its os/arch yet"}),
            ));
        };
        let bytes = self
            .runners
            .read(os, arch, sha256)
            .map_err(|e| (409, json!({"ok": false, "error": e})))?;
        eprintln!(
            "ralphus-fleet: sending outpost {name} the {os}-{arch} runner ({} bytes)",
            bytes.len()
        );
        let sealed = key
            .seal(FLEET_TO_OUTPOST, &bytes)
            .map_err(|e| refused(500, &e))?;
        Ok(json!({"box": sealed}))
    }

    /// A pairing request from a new outpost (`docs/outpost.md#pairing`):
    /// record its public key against the pending code it proves it holds,
    /// and answer with this fleet's public key, the machine's name and
    /// `remote_root`, authenticated with the same code.
    ///
    /// # Errors
    /// This fleet has no identity or registry, the request is malformed, or
    /// no pending code verifies it.
    pub fn pair(&self, body: &Value) -> Result<Value, Refusal> {
        let (Some(identity), Some(registry)) = (&self.identity, &self.registry) else {
            return Err(refused(
                404,
                "this fleet does not pair outposts (it runs without a registry)",
            ));
        };
        let machine_key = body["machine_public_key"].as_str().unwrap_or_default();
        let nonce = body["nonce"].as_str().unwrap_or_default();
        let mac = body["mac"].as_str().unwrap_or_default();
        if body["protocol"].as_str() != Some(PAIR_PROTOCOL)
            || !outpost_keys::is_public_key(machine_key)
            || !outpost_wire::is_hex(nonce, 32)
        {
            return Err(refused(400, "a malformed pairing request"));
        }
        let paired = registry
            .complete_pairing(machine_key, |key| {
                outpost_keys::verify_pair_request(key, machine_key, nonce, mac)
            })
            .map_err(|e| {
                eprintln!("ralphus-fleet: refused a pairing request: {e}");
                refused(
                    401,
                    "the pairing code was not accepted: it is wrong, used, or expired; run \
                     `ralphus-fleet pair <machine>` for a new one",
                )
            })?;
        let Some(remote_root) = self.remote_roots.get(&paired.name) else {
            return Err(refused(
                409,
                "the paired machine is not an outpost machine in this fleet's configuration",
            ));
        };
        let fleet_key = identity.public_key();
        let answer_mac = outpost_keys::pair_answer_mac(
            &paired.key,
            nonce,
            machine_key,
            &fleet_key,
            &paired.name,
            remote_root,
        );
        eprintln!(
            "ralphus-fleet: paired outpost {} ({})",
            paired.name,
            outpost_keys::fingerprint(machine_key)
        );
        Ok(json!({
            "name": paired.name,
            "remote_root": remote_root,
            "fleet_public_key": fleet_key,
            "mac": answer_mac,
        }))
    }
}

/// The slot's live session, dropping (and failing the requests of) one not
/// heard from for [`STALE`].
fn expire(slot: &mut Slot) -> Option<&Session> {
    if slot
        .active
        .as_ref()
        .is_some_and(|a| a.last_seen.elapsed() > STALE)
    {
        if let Some(old) = slot.active.take() {
            for (_, waiter) in old.in_flight {
                let _ = waiter.send(Err("the outpost stopped answering".to_string()));
            }
        }
        for outgoing in slot.queue.drain(..) {
            let _ = outgoing
                .waiter
                .send(Err("the outpost stopped answering".to_string()));
        }
    }
    slot.active.as_ref()
}

fn not_connected(name: &str) -> String {
    format!(
        "outpost machine {name:?} is not connected; on the machine, check `ralphus-outpost \
         status` (or set it up once with `ralphus-fleet pair {name}` here and `ralphus-outpost \
         install --fleet <url> --code <code>` there)"
    )
}
