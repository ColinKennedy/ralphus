//! The fleet side of the outpost transport: machines that connect *out* to
//! this service with `ralphus-outpost connect`, poll it for Machine API
//! requests, run them and post the replies back (`docs/outpost.md`).
//!
//! [`Outposts::call`] queues one request for a machine and waits for its
//! reply; [`Outposts::hello`], [`Outposts::poll`] and [`Outposts::reply`]
//! serve the outpost's side of the conversation, authenticated and sealed
//! as [`crate::outpost_wire`] describes. Pairing secrets live only in this
//! process's memory, read from each machine's `outpost_token_env`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::FleetConfig;
use crate::outpost_wire::{self, FLEET_TO_OUTPOST, OUTPOST_TO_FLEET, SessionKey};

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
}

struct Pending {
    name: String,
    session: Session,
    created: Instant,
}

#[derive(Default)]
struct Slot {
    active: Option<Session>,
    queue: VecDeque<Outgoing>,
    info: Value,
}

#[derive(Default)]
struct State {
    slots: BTreeMap<String, Slot>,
    pending: Vec<Pending>,
    next_id: u64,
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
    state: Mutex<State>,
    wake: Condvar,
}

impl Outposts {
    /// Outposts for `config`'s outpost machines, with their pairing secrets
    /// read from `secret_of(<outpost_token_env>)`. A machine whose secret is
    /// unset or too short refuses every connection, and says why on stderr.
    #[must_use]
    pub fn new(config: &FleetConfig, secret_of: impl Fn(&str) -> Option<String>) -> Self {
        let mut secrets = BTreeMap::new();
        let mut slots = BTreeMap::new();
        for machine in config.machines.iter().filter(|m| m.is_outpost()) {
            let name = machine.display_name().to_string();
            let variable = machine.outpost_token_env.clone().unwrap_or_default();
            match secret_of(&variable).map(|s| s.trim().to_string()) {
                Some(secret) if outpost_wire::check_secret(&secret).is_ok() => {
                    secrets.insert(name.clone(), secret);
                }
                Some(_) => eprintln!(
                    "ralphus-fleet: outpost machine {name:?}: {variable} is shorter than {} \
                     characters; it will refuse every connection",
                    outpost_wire::MIN_SECRET_LEN
                ),
                None => eprintln!(
                    "ralphus-fleet: outpost machine {name:?}: {variable} is not set; it will \
                     refuse every connection"
                ),
            }
            slots.insert(name, Slot::default());
        }
        Self {
            secrets,
            state: Mutex::new(State {
                slots,
                ..State::default()
            }),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Every outpost machine and whether it is connected.
    #[must_use]
    pub fn list(&self) -> Vec<Value> {
        let mut state = self.lock();
        state
            .slots
            .iter_mut()
            .map(|(name, slot)| {
                let connected = expire(slot).is_some();
                json!({"name": name, "connected": connected, "info": slot.info})
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
    /// its proof.
    ///
    /// # Errors
    /// An unknown machine, a machine without a usable secret, or a malformed
    /// hello -- all answered alike, so a stranger learns nothing.
    pub fn hello(&self, body: &Value) -> Result<Value, Refusal> {
        let refusal = || refused(401, "the outpost was refused");
        let name = body["name"].as_str().unwrap_or_default();
        let outpost_nonce = body["nonce"].as_str().unwrap_or_default();
        if body["protocol"].as_str() != Some(outpost_wire::PROTOCOL)
            || !outpost_wire::is_hex(outpost_nonce, 32)
        {
            return Err(refusal());
        }
        let Some(secret) = self.secrets.get(name) else {
            eprintln!("ralphus-fleet: refused an outpost hello for {name:?}");
            return Err(refusal());
        };
        let random = |n| outpost_wire::random_hex(n).map_err(|e| refused(500, &e));
        let session = random(16)?;
        let fleet_nonce = random(32)?;
        let key = SessionKey::derive(secret, name, outpost_nonce, &fleet_nonce, &session)
            .map_err(|e| refused(500, &e))?;
        let proof = outpost_wire::fleet_proof(secret, name, outpost_nonce, &fleet_nonce, &session);
        let mut state = self.lock();
        state.pending.retain(|p| p.created.elapsed() < PENDING_TTL);
        while state.pending.iter().filter(|p| p.name == name).count() >= MAX_PENDING {
            if let Some(oldest) = state.pending.iter().position(|p| p.name == name) {
                state.pending.remove(oldest);
            }
        }
        state.pending.push(Pending {
            name: name.to_string(),
            session: Session {
                id: session.clone(),
                key: Arc::new(key),
                last_seen: Instant::now(),
                last_seq: 0,
                in_flight: BTreeMap::new(),
            },
            created: Instant::now(),
        });
        Ok(json!({"session": session, "nonce": fleet_nonce, "proof": proof}))
    }

    /// Find the session `id`, promoting a pending one whose first sealed
    /// message opened. Returns the machine name and the opened message.
    fn open(&self, state: &mut State, id: &str, sealed: &str) -> Result<(String, Value), Refusal> {
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

    /// A sealed poll: wait up to [`POLL_WAIT`] for requests and hand them
    /// over, sealed.
    ///
    /// # Errors
    /// An unknown session, a message that does not authenticate, or a
    /// replayed sequence number.
    pub fn poll(&self, body: &Value) -> Result<Value, Refusal> {
        let session = body["session"].as_str().unwrap_or_default().to_string();
        let sealed = body["box"].as_str().unwrap_or_default();
        let mut state = self.lock();
        let (name, message) = self.open(&mut state, &session, sealed)?;
        let seq = message["seq"].as_u64().unwrap_or(0);
        {
            let slot = state
                .slots
                .get_mut(&name)
                .ok_or_else(|| refused(401, "gone"))?;
            let active = slot.active.as_mut().ok_or_else(|| refused(401, "gone"))?;
            if seq <= active.last_seq {
                return Err(refused(401, "a replayed outpost poll"));
            }
            active.last_seq = seq;
            if message["info"].is_object() {
                slot.info = message["info"].clone();
            }
        }
        let deadline = Instant::now() + POLL_WAIT;
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
        let plain = json!({"seq": seq, "requests": requests}).to_string();
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
        let (name, message) = self.open(&mut state, &session, sealed)?;
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
        "outpost machine {name:?} is not connected; start `ralphus-outpost connect <fleet url> \
         --name {name} --token-env <VAR> --remote-root <path>` on it"
    )
}
