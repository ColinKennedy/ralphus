//! `ralphus-outpost`: serves one machine's Machine API for `ralphus-fleet`
//! without any inbound port on the machine.
//!
//! The outpost connects *out* to the fleet, proves it holds the machine's
//! pairing secret and checks that the fleet does too
//! ([`ralphus_fleet::outpost_wire`]), then long-polls for requests. Each
//! request runs on its own thread through the same machine-side verbs a
//! Windows machine runs over SSH ([`ralphus_runner::machine::execute`]), and
//! its reply is posted back sealed. Jobs (`exec`) run under this same
//! executable (`ralphus-outpost runner ...` carries `ralphus-runner`), so
//! nothing else has to be copied to the machine.
//!
//! The pairing secret is read from an environment variable and kept only in
//! memory; it never goes on the wire, a command line, a file or a log line.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ralphus_fleet::outpost_wire::{self, FLEET_TO_OUTPOST, OUTPOST_TO_FLEET, SessionKey};
use ralphus_runner::machine::{Context, execute, paths};
use serde_json::{Value, json};

/// How the outpost reaches its fleet and what it serves.
#[derive(Clone)]
pub struct Options {
    /// The fleet's base URL (`http://fleet:7900`).
    pub fleet_url: String,
    /// This machine's name in the fleet's configuration.
    pub name: String,
    /// The pairing secret. Never printed.
    pub secret: String,
    /// The machine's storage root; a request for any other is refused.
    pub remote_root: String,
    /// Program and leading arguments that reach the runner (this executable
    /// and `runner`).
    pub runner: Vec<String>,
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

fn log(message: &str) {
    eprintln!("ralphus-outpost: {message}");
}

/// An established session with the fleet.
struct Session {
    id: String,
    key: Arc<SessionKey>,
}

/// Why a round trip to the fleet failed.
enum Failure {
    /// The fleet no longer knows the session (it restarted, or replaced it):
    /// say hello again.
    Session(String),
    /// The fleet could not be reached or answered nonsense: wait, then say
    /// hello again.
    Transport(String),
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(ralphus_fleet::outposts::POLL_WAIT + Duration::from_secs(30))
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
        Err(ureq::Error::Status(code, _)) => Err(Failure::Transport(format!(
            "the fleet answered HTTP {code}"
        ))),
        Err(e) => Err(Failure::Transport(e.to_string())),
    }
}

/// Say hello and check the fleet's proof.
fn handshake(agent: &ureq::Agent, options: &Options) -> Result<Session, Failure> {
    let nonce = outpost_wire::random_hex(32).map_err(Failure::Transport)?;
    let url = format!("{}/v1/outpost/hello", options.fleet_url);
    let answer = post(
        agent,
        &url,
        &json!({"protocol": outpost_wire::PROTOCOL, "name": options.name, "nonce": nonce}),
    )?;
    let session = answer["session"].as_str().unwrap_or_default();
    let fleet_nonce = answer["nonce"].as_str().unwrap_or_default();
    let proof = answer["proof"].as_str().unwrap_or_default();
    if !outpost_wire::is_hex(session, 16) || !outpost_wire::is_hex(fleet_nonce, 32) {
        return Err(Failure::Transport(
            "the fleet's hello answer is malformed".to_string(),
        ));
    }
    if !outpost_wire::verify_fleet_proof(
        &options.secret,
        &options.name,
        &nonce,
        fleet_nonce,
        session,
        proof,
    ) {
        return Err(Failure::Session(
            "the service at this URL could not prove it holds the pairing secret; refusing to \
             take requests from it"
                .to_string(),
        ));
    }
    let key = SessionKey::derive(&options.secret, &options.name, &nonce, fleet_nonce, session)
        .map_err(Failure::Transport)?;
    Ok(Session {
        id: session.to_string(),
        key: Arc::new(key),
    })
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

/// Connect and serve until `stop` is set. Reconnects on its own after the
/// fleet restarts or the network drops.
///
/// # Errors
/// The options are unusable (a short secret, a bad remote root).
pub fn run(options: &Options, stop: &AtomicBool) -> Result<(), String> {
    outpost_wire::check_secret(&options.secret)?;
    let root = paths::Root::new(&options.remote_root)?;
    std::fs::create_dir_all(root.as_str())
        .map_err(|e| format!("could not create {}: {e}", root.as_str()))?;
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
                        "could not connect to {}: {reason}",
                        options.fleet_url
                    ));
                    announced_failure = true;
                }
                sleep_unless_stopped(backoff, stop);
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        log(&format!(
            "connected to {} as {:?}, serving {}",
            options.fleet_url,
            options.name,
            root.as_str()
        ));
        announced_failure = false;
        backoff = Duration::from_secs(1);
        if let Some(reason) = serve_session(&agent, options, &root, &session, &seen, stop) {
            log(&format!("connection lost ({reason}); reconnecting"));
            sleep_unless_stopped(Duration::from_secs(1), stop);
        }
        // Request ids are per fleet process; a new session may restart them.
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
    Ok(())
}

fn sleep_unless_stopped(duration: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < duration && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(step);
        slept += step;
    }
}

/// Poll one session until it fails or `stop` is set; the reason it ended.
fn serve_session(
    agent: &ureq::Agent,
    options: &Options,
    root: &paths::Root,
    session: &Session,
    seen: &Arc<Mutex<BTreeSet<u64>>>,
    stop: &AtomicBool,
) -> Option<String> {
    let poll_url = format!("{}/v1/outpost/poll", options.fleet_url);
    let mut seq: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        seq += 1;
        let mut message = json!({"seq": seq});
        if seq == 1 {
            message["info"] = info();
        }
        let sealed = match session
            .key
            .seal(OUTPOST_TO_FLEET, message.to_string().as_bytes())
        {
            Ok(sealed) => sealed,
            Err(e) => return Some(e),
        };
        let answer = match post(
            agent,
            &poll_url,
            &json!({"session": session.id, "box": sealed}),
        ) {
            Ok(answer) => answer,
            Err(Failure::Session(reason) | Failure::Transport(reason)) => return Some(reason),
        };
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
        for request in opened["requests"].as_array().cloned().unwrap_or_default() {
            let Some(id) = request["id"].as_u64() else {
                continue;
            };
            if !seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id)
            {
                continue;
            }
            let agent = agent.clone();
            let options = options.clone();
            let root = root.clone();
            let session_id = session.id.clone();
            let key = Arc::clone(&session.key);
            std::thread::spawn(move || {
                answer_request(&agent, &options, &root, &session_id, &key, id, &request);
            });
        }
    }
    None
}

/// Run one request and post its sealed reply.
fn answer_request(
    agent: &ureq::Agent,
    options: &Options,
    root: &paths::Root,
    session: &str,
    key: &SessionKey,
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
                runner: options.runner.clone(),
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
