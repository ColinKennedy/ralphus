//! `ralphus-outpost`: serves one machine's Machine API for `ralphus-fleet`
//! without any inbound port on the machine.
//!
//! The outpost connects *out* to the fleet and authenticates both ways --
//! with a pairing secret from the environment ([`Auth::Secret`],
//! [`ralphus_fleet::outpost_wire`]) or with the machine credential it got by
//! pairing ([`Auth::Paired`], [`ralphus_fleet::outpost_keys`]) -- then
//! long-polls for requests. Each request runs on its own thread through the
//! same machine-side verbs a Windows machine runs over SSH
//! ([`ralphus_runner::machine::execute`]), and its reply is posted back
//! sealed.
//!
//! Every poll answer says what the fleet expects of this machine: the
//! agents to check ([`readiness`], reported back in a later poll) and the
//! runner to run jobs with ([`runner_cache`]: downloaded, verified by
//! sha256 and swapped in for new jobs). Until a fleet names a runner, jobs
//! run under this same executable (`ralphus-outpost runner ...` carries
//! `ralphus-runner`).
//!
//! It reconnects forever: after a failure it waits with capped exponential
//! backoff plus jitter, and it notices a dead connection within
//! [`DEAD_AFTER`] (each long poll is a keepalive) and a sleep/wake (the wall
//! clock jumping ahead of the monotonic one), reconnecting at once.
//!
//! Secrets -- the pairing secret, the machine credential, session keys --
//! are kept in memory only; they never go on the wire, a command line, a
//! file or a log line.

pub mod install;
pub mod logfile;
pub mod pairing;
pub mod readiness;
pub mod runner_cache;
pub mod settings;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ralphus_fleet::outpost_keys::{self, Ephemeral, Identity, PROTOCOL_V2};
use ralphus_fleet::outpost_wire::{self, FLEET_TO_OUTPOST, OUTPOST_TO_FLEET, SessionKey};
use ralphus_runner::machine::{Context, execute, paths};
use serde_json::{Value, json};

/// A connection silent for this long is dead: the fleet answers every long
/// poll within [`ralphus_fleet::outposts::POLL_WAIT`].
pub const DEAD_AFTER: Duration = Duration::from_secs(30);
/// The longest wait between reconnection attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The wall clock running this far ahead of the monotonic clock means the
/// machine slept.
const SLEEP_JUMP: Duration = Duration::from_secs(15);

/// How the outpost proves who it is.
#[derive(Clone)]
pub enum Auth {
    /// A shared pairing secret (`outpost_token_env` on the fleet).
    Secret(String),
    /// A paired machine credential: this machine's key and the fleet's
    /// public key it pinned when pairing.
    Paired {
        machine: Arc<Identity>,
        fleet_public_key: String,
    },
}

/// How the outpost reaches its fleet and what it serves.
#[derive(Clone)]
pub struct Options {
    /// The fleet's base URL (`http://fleet:7900`).
    pub fleet_url: String,
    /// This machine's name in the fleet's configuration.
    pub name: String,
    /// How it authenticates. Never printed.
    pub auth: Auth,
    /// The machine's storage root; a request for any other is refused.
    pub remote_root: String,
    /// Program and leading arguments that reach the built-in runner (this
    /// executable and `runner`), used until the fleet names one.
    pub runner: Vec<String>,
    /// Where downloaded runners are kept; `None` keeps using the built-in
    /// one whatever the fleet says.
    pub runner_cache: Option<std::path::PathBuf>,
    /// Whether to check the machine ([`readiness`]) and report it.
    pub check_readiness: bool,
    /// Where to record the connection state for `ralphus-outpost status`.
    pub status_file: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("fleet_url", &self.fleet_url)
            .field("name", &self.name)
            .field("remote_root", &self.remote_root)
            .finish_non_exhaustive()
    }
}

/// Write `message` to stderr and the log file, if one is open.
pub fn log(message: &str) {
    logfile::line(&format!("ralphus-outpost: {message}"));
}

/// An established session with the fleet.
struct Session {
    id: String,
    key: Arc<SessionKey>,
    /// The machine's signature of the hello, sent with the first poll
    /// (paired sessions only).
    proof: Option<String>,
}

/// Why a round trip to the fleet failed.
enum Failure {
    /// The fleet no longer knows the session (it restarted, replaced it, or
    /// refused it): say hello again.
    Session(String),
    /// The fleet could not be reached or answered nonsense: wait, then say
    /// hello again.
    Transport(String),
}

/// State the connection loop and the request threads share.
struct Shared {
    /// The runner new jobs start with.
    runner: Mutex<Vec<String>>,
    /// A readiness report waiting to go out with the next poll.
    readiness: Mutex<Option<ralphus_fleet::readiness::Report>>,
    /// The agents the last check covered, so a change re-checks.
    checked_agents: Mutex<Option<Value>>,
    checking: AtomicBool,
    updating_runner: AtomicBool,
    cache: Option<runner_cache::RunnerCache>,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(ralphus_fleet::outposts::POLL_WAIT + Duration::from_secs(10))
        .timeout_write(Duration::from_secs(60))
        .build()
}

fn post(agent: &ureq::Agent, url: &str, body: &Value) -> Result<Value, Failure> {
    match agent
        .post(url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(response) => {
            let text = response
                .into_string()
                .map_err(|e| Failure::Transport(e.to_string()))?;
            serde_json::from_str(&text)
                .map_err(|e| Failure::Transport(format!("the fleet sent unparseable JSON: {e}")))
        }
        Err(ureq::Error::Status(401 | 404, response)) => {
            let text = response.into_string().unwrap_or_default();
            let reason = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or(text);
            Err(Failure::Session(reason))
        }
        Err(ureq::Error::Status(code, response)) => {
            let text = response.into_string().unwrap_or_default();
            let reason = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_default();
            Err(Failure::Transport(format!(
                "the fleet answered HTTP {code} {reason}"
            )))
        }
        Err(e) => Err(Failure::Transport(e.to_string())),
    }
}

/// Say hello and check the fleet's proof.
fn handshake(agent: &ureq::Agent, options: &Options) -> Result<Session, Failure> {
    let nonce = outpost_wire::random_hex(32).map_err(Failure::Transport)?;
    let url = format!("{}/v1/outpost/hello", options.fleet_url);
    match &options.auth {
        Auth::Secret(secret) => {
            let answer = post(
                agent,
                &url,
                &json!({"protocol": outpost_wire::PROTOCOL, "name": options.name, "nonce": nonce}),
            )?;
            let session = answer["session"].as_str().unwrap_or_default();
            let fleet_nonce = answer["nonce"].as_str().unwrap_or_default();
            let proof = answer["proof"].as_str().unwrap_or_default();
            check_hello_shape(session, fleet_nonce)?;
            if !outpost_wire::verify_fleet_proof(
                secret,
                &options.name,
                &nonce,
                fleet_nonce,
                session,
                proof,
            ) {
                return Err(Failure::Session(
                    "the service at this URL could not prove it holds the pairing secret; \
                     refusing to take requests from it"
                        .to_string(),
                ));
            }
            let key = SessionKey::derive(secret, &options.name, &nonce, fleet_nonce, session)
                .map_err(Failure::Transport)?;
            Ok(Session {
                id: session.to_string(),
                key: Arc::new(key),
                proof: None,
            })
        }
        Auth::Paired {
            machine,
            fleet_public_key,
        } => {
            let ephemeral = Ephemeral::generate().map_err(Failure::Transport)?;
            let outpost_ephemeral = ephemeral.public().to_string();
            let answer = post(
                agent,
                &url,
                &json!({
                    "protocol": PROTOCOL_V2,
                    "name": options.name,
                    "nonce": nonce,
                    "ephemeral": outpost_ephemeral,
                }),
            )?;
            let session = answer["session"].as_str().unwrap_or_default();
            let fleet_nonce = answer["nonce"].as_str().unwrap_or_default();
            let fleet_ephemeral = answer["ephemeral"].as_str().unwrap_or_default();
            let signature = answer["signature"].as_str().unwrap_or_default();
            check_hello_shape(session, fleet_nonce)?;
            let transcript = |role| {
                outpost_keys::transcript_v2(
                    role,
                    &options.name,
                    &nonce,
                    &outpost_ephemeral,
                    fleet_nonce,
                    fleet_ephemeral,
                    session,
                )
            };
            if !outpost_keys::verify_signature(fleet_public_key, &transcript("fleet"), signature) {
                return Err(Failure::Session(
                    "the service at this URL is not the fleet this machine paired with; \
                     refusing to take requests from it"
                        .to_string(),
                ));
            }
            let shared = ephemeral
                .agree(fleet_ephemeral)
                .map_err(Failure::Transport)?;
            let key = SessionKey::derive_v2(&shared, &options.name, &nonce, fleet_nonce, session)
                .map_err(Failure::Transport)?;
            Ok(Session {
                id: session.to_string(),
                key: Arc::new(key),
                proof: Some(machine.sign(&transcript("outpost"))),
            })
        }
    }
}

fn check_hello_shape(session: &str, fleet_nonce: &str) -> Result<(), Failure> {
    if outpost_wire::is_hex(session, 16) && outpost_wire::is_hex(fleet_nonce, 32) {
        Ok(())
    } else {
        Err(Failure::Transport(
            "the fleet's hello answer is malformed".to_string(),
        ))
    }
}

/// What this machine says about itself when it connects.
fn info() -> Value {
    json!({
        "version": ralphus_runner::machine::PROTOCOL_VERSION,
        "outpost_version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    })
}

/// The next backoff: doubled, capped at [`MAX_BACKOFF`].
#[must_use]
pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// `backoff` with jitter: a random duration in `[backoff / 2, backoff]`, so
/// many outposts that lost the same fleet do not all return at once.
#[must_use]
pub fn jittered(backoff: Duration) -> Duration {
    let mut byte = [0_u8; 2];
    let fraction = if getrandom::getrandom(&mut byte).is_ok() {
        f64::from(u16::from_le_bytes(byte)) / f64::from(u16::MAX)
    } else {
        1.0
    };
    backoff / 2 + backoff.mul_f64(fraction / 2.0)
}

/// Watches for the machine sleeping: the wall clock moving further than
/// the monotonic clock between two looks.
struct SleepWatch {
    instant: Instant,
    wall: SystemTime,
}

impl SleepWatch {
    fn new() -> Self {
        Self {
            instant: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    /// Whether the machine slept since the last look; restarts the watch.
    fn slept(&mut self) -> bool {
        let monotonic = self.instant.elapsed();
        let wall = self.wall.elapsed().unwrap_or_default();
        *self = Self::new();
        wall > monotonic + SLEEP_JUMP
    }
}

/// Connect and serve until `stop` is set. Reconnects on its own after the
/// fleet restarts, the network drops or the machine wakes from sleep.
///
/// # Errors
/// The options are unusable (a short secret, a bad remote root).
pub fn run(options: &Options, stop: &AtomicBool) -> Result<(), String> {
    if let Auth::Secret(secret) = &options.auth {
        outpost_wire::check_secret(secret)?;
    }
    let root = paths::Root::new(&options.remote_root)?;
    std::fs::create_dir_all(root.as_str())
        .map_err(|e| format!("could not create {}: {e}", root.as_str()))?;
    let cache = options
        .runner_cache
        .as_ref()
        .map(|dir| runner_cache::RunnerCache::new(dir.clone(), root.as_str()));
    let start_runner = cache
        .as_ref()
        .and_then(runner_cache::RunnerCache::current)
        .map_or_else(|| options.runner.clone(), |path| vec![path]);
    let shared = Arc::new(Shared {
        runner: Mutex::new(start_runner),
        readiness: Mutex::new(None),
        checked_agents: Mutex::new(None),
        checking: AtomicBool::new(false),
        updating_runner: AtomicBool::new(false),
        cache,
    });
    let agent = agent();
    let seen = Arc::new(Mutex::new(BTreeSet::<u64>::new()));
    let mut backoff = Duration::from_secs(1);
    let mut announced_failure = false;
    while !stop.load(Ordering::Relaxed) {
        let session = match handshake(&agent, options) {
            Ok(session) => session,
            Err(Failure::Session(reason) | Failure::Transport(reason)) => {
                if !announced_failure {
                    log(&format!(
                        "could not connect to {}: {reason} (retrying with backoff)",
                        options.fleet_url
                    ));
                    record_status(options, false, Some(&reason));
                    announced_failure = true;
                }
                sleep_unless_stopped(jittered(backoff), stop);
                backoff = next_backoff(backoff);
                continue;
            }
        };
        log(&format!(
            "connected to {} as {:?}, serving {}",
            options.fleet_url,
            options.name,
            root.as_str()
        ));
        record_status(options, true, None);
        announced_failure = false;
        backoff = Duration::from_secs(1);
        // Check the machine afresh on each connection.
        *lock(&shared.checked_agents) = None;
        if let Some(reason) = serve_session(&agent, options, &root, &session, &seen, &shared, stop)
        {
            log(&format!("connection lost ({reason}); reconnecting"));
            record_status(options, false, Some(&reason));
            sleep_unless_stopped(jittered(Duration::from_secs(1)), stop);
        }
        // Request ids are per fleet process; a new session may restart them.
        lock(&seen).clear();
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn record_status(options: &Options, connected: bool, error: Option<&str>) {
    if let Some(path) = &options.status_file {
        let status = json!({
            "connected": connected,
            "fleet_url": options.fleet_url,
            "name": options.name,
            "at_ms": ralphus_fleet::allocations::now_ms(),
            "error": error,
            "pid": std::process::id(),
        });
        let _ = ralphus_fleet::secret_store::write_private(path, status.to_string().as_bytes());
    }
}

/// Sleep for `duration`, waking early when `stop` is set or the machine
/// slept (so a wake reconnects at once).
fn sleep_unless_stopped(duration: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(100);
    let started = Instant::now();
    let mut watch = SleepWatch::new();
    while started.elapsed() < duration && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(step);
        if watch.slept() {
            return;
        }
    }
}

/// Poll one session until it fails or `stop` is set; the reason it ended.
fn serve_session(
    agent: &ureq::Agent,
    options: &Options,
    root: &paths::Root,
    session: &Session,
    seen: &Arc<Mutex<BTreeSet<u64>>>,
    shared: &Arc<Shared>,
    stop: &AtomicBool,
) -> Option<String> {
    let poll_url = format!("{}/v1/outpost/poll", options.fleet_url);
    let mut seq: u64 = 0;
    let mut watch = SleepWatch::new();
    while !stop.load(Ordering::Relaxed) {
        seq += 1;
        let mut message = json!({"seq": seq});
        if seq == 1 {
            message["info"] = info();
        }
        let report = lock(&shared.readiness).take();
        if let Some(report) = &report {
            message["readiness"] = serde_json::to_value(report).unwrap_or(Value::Null);
        }
        let sealed = match session
            .key
            .seal(OUTPOST_TO_FLEET, message.to_string().as_bytes())
        {
            Ok(sealed) => sealed,
            Err(e) => return Some(e),
        };
        let mut body = json!({"session": session.id, "box": sealed});
        if seq == 1 {
            if let Some(proof) = &session.proof {
                body["proof"] = proof.clone().into();
            }
        }
        let answer = match post(agent, &poll_url, &body) {
            Ok(answer) => answer,
            Err(Failure::Session(reason) | Failure::Transport(reason)) => {
                if let Some(report) = report {
                    lock(&shared.readiness).get_or_insert(report);
                }
                return Some(if watch.slept() {
                    format!("the machine slept: {reason}")
                } else {
                    reason
                });
            }
        };
        if watch.slept() {
            return Some("the machine slept; starting a fresh session".to_string());
        }
        let Some(opened) = answer["box"]
            .as_str()
            .and_then(|b| session.key.open(FLEET_TO_OUTPOST, b).ok())
            .and_then(|plain| serde_json::from_slice::<Value>(&plain).ok())
        else {
            return Some("the fleet's answer did not authenticate".to_string());
        };
        if opened["seq"].as_u64() != Some(seq) {
            return Some("the fleet's answer was for another poll".to_string());
        }
        act_on_expectation(agent, options, root, session, shared, &opened["expect"]);
        for request in opened["requests"].as_array().cloned().unwrap_or_default() {
            let Some(id) = request["id"].as_u64() else {
                continue;
            };
            if !lock(seen).insert(id) {
                continue;
            }
            let agent = agent.clone();
            let options = options.clone();
            let root = root.clone();
            let session_id = session.id.clone();
            let key = Arc::clone(&session.key);
            let runner = lock(&shared.runner).clone();
            std::thread::spawn(move || {
                answer_request(
                    &agent,
                    &options,
                    &root,
                    &session_id,
                    &key,
                    &runner,
                    id,
                    &request,
                );
            });
        }
    }
    None
}

/// Start whatever the fleet's expectation asks for that is not under way:
/// a readiness check for a new set of agents, a runner download for a new
/// runner hash.
fn act_on_expectation(
    agent: &ureq::Agent,
    options: &Options,
    root: &paths::Root,
    session: &Session,
    shared: &Arc<Shared>,
    expect: &Value,
) {
    if options.check_readiness {
        let agents = expect["agents"].clone();
        let changed = lock(&shared.checked_agents).as_ref() != Some(&agents);
        if changed && !shared.checking.swap(true, Ordering::SeqCst) {
            *lock(&shared.checked_agents) = Some(agents.clone());
            let shared = Arc::clone(shared);
            let root = root.as_str().to_string();
            std::thread::spawn(move || {
                let report =
                    readiness::check(&root, agents.as_object().cloned().unwrap_or_default());
                let problems = report.problems();
                if problems.is_empty() {
                    log("readiness: every check passed");
                } else {
                    for check in problems {
                        log(&format!(
                            "readiness: {} {}: {}",
                            check.status, check.id, check.detail
                        ));
                    }
                }
                *lock(&shared.readiness) = Some(report);
                shared.checking.store(false, Ordering::SeqCst);
            });
        }
    }
    let Some(cache) = &shared.cache else {
        return;
    };
    let wanted = expect["runner"]["sha256"].as_str().map(str::to_string);
    let Some(wanted) = wanted else {
        // No runner for this platform: jobs use the built-in one.
        *lock(&shared.runner) = options.runner.clone();
        return;
    };
    if let Some(path) = cache.installed(&wanted) {
        let mut current = lock(&shared.runner);
        if *current != [path.clone()] {
            if cache.set_current(&wanted).is_ok() {
                log(&format!("running new jobs with runner {}", &wanted[..12]));
            }
            *current = vec![path];
        }
        return;
    }
    if shared.updating_runner.swap(true, Ordering::SeqCst) {
        return;
    }
    let agent = agent.clone();
    let url = format!("{}/v1/outpost/runner", options.fleet_url);
    let session_id = session.id.clone();
    let key = Arc::clone(&session.key);
    let shared = Arc::clone(shared);
    std::thread::spawn(move || {
        let result = download_runner(&agent, &url, &session_id, &key, &wanted).and_then(|bytes| {
            let cache = shared.cache.as_ref().ok_or("no runner cache")?;
            let path = cache.install(&wanted, &bytes)?;
            cache.set_current(&wanted)?;
            Ok(path)
        });
        match result {
            Ok(path) => {
                log(&format!(
                    "runner {} verified and installed; new jobs use it",
                    &wanted[..12]
                ));
                *lock(&shared.runner) = vec![path];
                if let Some(cache) = &shared.cache {
                    cache.collect_garbage();
                }
            }
            Err(e) => log(&format!("could not update the runner: {e}")),
        }
        shared.updating_runner.store(false, Ordering::SeqCst);
    });
}

fn download_runner(
    agent: &ureq::Agent,
    url: &str,
    session: &str,
    key: &SessionKey,
    sha256: &str,
) -> Result<Vec<u8>, String> {
    let sealed = key.seal(
        OUTPOST_TO_FLEET,
        json!({"sha256": sha256}).to_string().as_bytes(),
    )?;
    let answer = match post(agent, url, &json!({"session": session, "box": sealed})) {
        Ok(answer) => answer,
        Err(Failure::Session(e) | Failure::Transport(e)) => return Err(e),
    };
    let bytes = answer["box"]
        .as_str()
        .ok_or("the fleet sent no runner")
        .and_then(|b| {
            key.open(FLEET_TO_OUTPOST, b)
                .map_err(|_| "the runner did not authenticate")
        })?;
    let got = ralphus_fleet::runner_artifacts::sha256_hex(&bytes);
    if got != sha256 {
        return Err(format!(
            "the runner's sha256 is {got}, not the expected {sha256}; not installing it"
        ));
    }
    Ok(bytes)
}

/// Run one request and post its sealed reply.
#[allow(clippy::too_many_arguments)]
fn answer_request(
    agent: &ureq::Agent,
    options: &Options,
    root: &paths::Root,
    session: &str,
    key: &SessionKey,
    runner: &[String],
    id: u64,
    request: &Value,
) {
    let verb = request["verb"].as_str().unwrap_or_default();
    log(&format!("request {id}: {verb}"));
    let asked_root = request["context"]["remote_root"]
        .as_str()
        .unwrap_or_default();
    let reply = match paths::Root::new(asked_root) {
        Ok(asked) if same_root(&asked, root) => execute(
            verb,
            &request["body"],
            &Context {
                remote_root: root.as_str().to_string(),
                runner: runner.to_vec(),
            },
        ),
        _ => ralphus_runner::machine::Reply {
            body: json!({
                "ok": false,
                "error": format!(
                    "the fleet asked for remote_root {asked_root:?}, but this outpost serves {:?}; \
                     make the fleet's remote_root for {:?} match the outpost's --remote-root",
                    root.as_str(),
                    options.name
                ),
            }),
            events: Vec::new(),
        },
    };
    if reply.body["ok"] != true {
        log(&format!("request {id}: {verb} refused"));
    }
    let message = json!({"id": id, "reply": reply.body, "events": reply.events});
    let Ok(sealed) = key.seal(OUTPOST_TO_FLEET, message.to_string().as_bytes()) else {
        log(&format!("request {id}: could not seal the reply"));
        return;
    };
    let url = format!("{}/v1/outpost/reply", options.fleet_url);
    if let Err(Failure::Session(reason) | Failure::Transport(reason)) =
        post(agent, &url, &json!({"session": session, "box": sealed}))
    {
        log(&format!(
            "request {id}: could not deliver the reply: {reason}"
        ));
    }
}

fn same_root(a: &paths::Root, b: &paths::Root) -> bool {
    if cfg!(windows) {
        a.as_str().eq_ignore_ascii_case(b.as_str())
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_a_cap_and_jitter_stays_within_half_to_full() {
        let mut backoff = Duration::from_secs(1);
        for _ in 0..20 {
            backoff = next_backoff(backoff);
        }
        assert_eq!(backoff, MAX_BACKOFF);
        for _ in 0..200 {
            let j = jittered(Duration::from_secs(8));
            assert!(
                j >= Duration::from_secs(4) && j <= Duration::from_secs(8),
                "{j:?}"
            );
        }
    }

    #[test]
    fn a_dead_connection_is_noticed_well_before_the_fleet_drops_the_session() {
        assert!(
            ralphus_fleet::outposts::POLL_WAIT + Duration::from_secs(10) <= DEAD_AFTER,
            "the read timeout is the keepalive deadline"
        );
    }
}
