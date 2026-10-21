//! `ralphus-fleet` and `ralphus-outpost` over loopback: an outpost connects
//! out, the fleet hands it Machine API requests for an allocation, and the
//! replies come back -- and an outpost or a fleet without the right pairing
//! secret or paired credential gets nothing. A paired outpost is kept on
//! the fleet's runner and reports its readiness.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ralphus_fleet::allocations::Allocations;
use ralphus_fleet::config::FleetConfig;
use ralphus_fleet::outpost_keys::Identity;
use ralphus_fleet::outpost_registry::Registry;
use ralphus_fleet::outposts::Outposts;
use ralphus_fleet::runner_artifacts::sha256_hex;
use ralphus_fleet::server::{Service, serve_on};
use ralphus_outpost::Auth;
use ralphus_outpost::runner_cache::RunnerCache;
use serde_json::{Value, json};

const SERVICE_TOKEN: &str = "fleet-service-token";
const SECRET: &str = "0123456789abcdef0123456789abcdef-loopback";

struct Fleet {
    url: String,
    server: Arc<tiny_http::Server>,
}

impl Drop for Fleet {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

fn fleet(remote_root: &str, secret: &'static str) -> Fleet {
    let config = FleetConfig::parse(
        &format!(
            "token_env = \"UNUSED\"\n[[machine]]\nname = \"box\"\ntransport = \"outpost\"\n\
             outpost_token_env = \"BOX_SECRET\"\nlabels = [\"outpost\"]\nremote_root = {remote_root:?}\n"
        ),
        "fleet.toml",
    )
    .unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let service = Service::with_outpost_secrets(
        config,
        url.clone(),
        SERVICE_TOKEN.to_string(),
        Allocations::default(),
        move |variable| (variable == "BOX_SECRET").then(|| secret.to_string()),
    );
    let serving = Arc::clone(&server);
    std::thread::spawn(move || serve_on(&serving, service));
    Fleet { url, server }
}

/// Stops its outpost when dropped; the outpost thread finishes on its own
/// once its current long poll returns.
struct Outpost {
    stop: Arc<AtomicBool>,
}

impl Drop for Outpost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn outpost(url: &str, remote_root: &str, secret: &str) -> Outpost {
    outpost_with(
        url,
        remote_root,
        Auth::Secret(secret.to_string()),
        None,
        false,
    )
}

fn outpost_with(
    url: &str,
    remote_root: &str,
    auth: Auth,
    runner_cache: Option<PathBuf>,
    check_readiness: bool,
) -> Outpost {
    let options = ralphus_outpost::Options {
        fleet_url: url.to_string(),
        name: "box".to_string(),
        auth,
        remote_root: remote_root.to_string(),
        runner: vec!["ralphus-runner-not-needed".to_string()],
        runner_cache,
        check_readiness,
        status_file: None,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        let _ = ralphus_outpost::run(&options, &flag);
    });
    Outpost { stop }
}

fn call(method: &str, url: &str, token: &str, body: &Value) -> (u16, Value) {
    let request = ureq::request(method, url).set("Authorization", &format!("Bearer {token}"));
    let result = if method == "GET" {
        request.call()
    } else {
        request.send_string(&body.to_string())
    };
    let (status, text) = match result {
        Ok(r) => (r.status(), r.into_string().unwrap()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    };
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn outposts_list(fleet: &Fleet) -> Value {
    call(
        "GET",
        &format!("{}/v1/outposts", fleet.url),
        SERVICE_TOKEN,
        &Value::Null,
    )
    .1
}

fn connected(fleet: &Fleet) -> bool {
    outposts_list(fleet)["outposts"][0]["connected"] == true
}

fn wait_connected(fleet: &Fleet) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !connected(fleet) {
        assert!(Instant::now() < deadline, "the outpost never connected");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn allocate(fleet: &Fleet) -> (String, String) {
    let (status, granted) = try_allocate(fleet, "s");
    assert_eq!(status, 200, "{granted}");
    (
        granted["endpoint"].as_str().unwrap().to_string(),
        granted["token"].as_str().unwrap().to_string(),
    )
}

fn try_allocate(fleet: &Fleet, squad: &str) -> (u16, Value) {
    call(
        "POST",
        &format!("{}/v1/allocations", fleet.url),
        SERVICE_TOKEN,
        &json!({"requirements": {"labels": ["outpost"]}, "scope": {"squad_id": squad, "key": "k"}}),
    )
}

/// A fleet that pairs: its registry in `dir`, its own identity, and `top`
/// (top-level keys) and `machine` (keys of the one machine, `box`) added to
/// the configuration.
fn paired_fleet(dir: &Path, remote_root: &str, top: &str, machine: &str) -> (Fleet, Registry) {
    let config = FleetConfig::parse(
        &format!(
            "token_env = \"UNUSED\"\n{top}\n[[machine]]\nname = \"box\"\ntransport = \"outpost\"\n\
             labels = [\"outpost\"]\nremote_root = {remote_root:?}\n{machine}\n"
        ),
        "fleet.toml",
    )
    .unwrap();
    let registry = Registry::new(dir.join("fleet.toml.outposts.json"));
    let outposts = Outposts::new(&config, |_| None)
        .with_registry(registry.clone())
        .with_identity(Identity::generate().unwrap());
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let service = Service::with_outposts(
        config,
        url.clone(),
        SERVICE_TOKEN.to_string(),
        Allocations::default(),
        outposts,
    );
    let serving = Arc::clone(&server);
    std::thread::spawn(move || serve_on(&serving, service));
    (Fleet { url, server }, registry)
}

fn paired_auth(credential: &ralphus_outpost::pairing::Credential) -> Auth {
    Auth::Paired {
        machine: Arc::clone(&credential.machine),
        fleet_public_key: credential.fleet_public_key.clone(),
    }
}

/// Make the outpost poll again (each answered request ends its long poll)
/// until `done`, or fail with `what`.
fn poll_until(fleet: &Fleet, what: &str, done: impl Fn() -> bool) {
    let (endpoint, token) = allocate(fleet);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        let _ = call("POST", &format!("{endpoint}/ping"), &token, &json!({}));
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_pairing_code_pairs_once_and_its_credential_connects_until_revoked() {
    let dir = ralphus_test_support::TestDir::new("outpost-paired");
    let root = root_of(&dir.join("root"));
    let (fleet, registry) = paired_fleet(&dir, &root, "", "");
    let code = registry.start_pairing("box").unwrap();
    let paired = ralphus_outpost::pairing::pair(&fleet.url, &code).unwrap();
    assert_eq!(paired.name, "box");
    assert_eq!(paired.remote_root, root);
    let again = ralphus_outpost::pairing::pair(&fleet.url, &code).unwrap_err();
    assert!(
        again.contains("not accepted"),
        "a code is single use: {again}"
    );
    let stored = std::fs::read_to_string(registry.path()).unwrap();
    assert!(!stored.contains(&paired.credential.machine.private_b64()));

    let _outpost = outpost_with(
        &fleet.url,
        &root,
        paired_auth(&paired.credential),
        None,
        false,
    );
    wait_connected(&fleet);
    assert_eq!(outposts_list(&fleet)["outposts"][0]["paired"], true);
    let (endpoint, token) = allocate(&fleet);
    let (_, ping) = call("POST", &format!("{endpoint}/ping"), &token, &json!({}));
    assert_eq!(ping["ok"], true, "{ping}");

    assert!(registry.revoke("box").unwrap());
    let deadline = Instant::now() + Duration::from_secs(40);
    while connected(&fleet) {
        assert!(
            Instant::now() < deadline,
            "a revoked outpost stays connected"
        );
        let _ = call("POST", &format!("{endpoint}/ping"), &token, &json!({}));
        std::thread::sleep(Duration::from_millis(200));
    }
    let (_, ping) = call("POST", &format!("{endpoint}/ping"), &token, &json!({}));
    assert_eq!(ping["reason_code"], "unreachable", "{ping}");
}

#[test]
fn a_paired_outpost_refuses_a_fleet_that_is_not_the_one_it_paired_with() {
    let dir = ralphus_test_support::TestDir::new("outpost-wrong-fleet");
    let root = root_of(&dir.join("root"));
    let (fleet, registry) = paired_fleet(&dir, &root, "", "");
    let machine = Arc::new(Identity::generate().unwrap());
    let public_key = machine.public_key();
    registry
        .update(|file| {
            file.machines
                .entry("box".to_string())
                .or_default()
                .public_key = Some(public_key);
            Ok(())
        })
        .unwrap();
    let impostor_pinned = Auth::Paired {
        machine,
        fleet_public_key: Identity::generate().unwrap().public_key(),
    };
    let _outpost = outpost_with(&fleet.url, &root, impostor_pinned, None, false);
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !connected(&fleet),
        "the outpost must not serve an unknown fleet"
    );
}

#[test]
fn a_paired_outpost_keeps_the_fleets_runner_and_rolls_forward_when_it_changes() {
    let dir = ralphus_test_support::TestDir::new("outpost-runner-update");
    let root = root_of(&dir.join("root"));
    let artifact = dir.join("ralphus-runner-artifact");
    std::fs::write(&artifact, b"runner version one").unwrap();
    let top = format!(
        "[outpost_runners]\n\"{}-{}\" = {:?}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        root_of(&artifact)
    );
    let (fleet, registry) = paired_fleet(&dir, &root, &top, "");
    let paired =
        ralphus_outpost::pairing::pair(&fleet.url, &registry.start_pairing("box").unwrap())
            .unwrap();
    let cache_dir = dir.join("cache");
    let _outpost = outpost_with(
        &fleet.url,
        &root,
        paired_auth(&paired.credential),
        Some(cache_dir.clone()),
        false,
    );
    wait_connected(&fleet);
    let cache = RunnerCache::new(cache_dir, &root);
    let one = sha256_hex(b"runner version one");
    poll_until(&fleet, "the first runner was never installed", || {
        cache.current() == cache.installed(&one) && cache.current().is_some()
    });

    std::fs::write(&artifact, b"runner version two, rebuilt").unwrap();
    let two = sha256_hex(b"runner version two, rebuilt");
    poll_until(&fleet, "the changed runner was never installed", || {
        cache.current().is_some() && cache.current() == cache.installed(&two)
    });
    assert!(
        cache.installed(&one).is_none(),
        "the old runner, used by no job, is removed"
    );
}

#[test]
fn a_machine_missing_a_required_agent_is_refused_with_the_fix() {
    let dir = ralphus_test_support::TestDir::new("outpost-readiness");
    let root = root_of(&dir.join("root"));
    let (fleet, registry) = paired_fleet(&dir, &root, "", "required_agents = [\"no-such-agent\"]");
    let paired =
        ralphus_outpost::pairing::pair(&fleet.url, &registry.start_pairing("box").unwrap())
            .unwrap();
    let _outpost = outpost_with(
        &fleet.url,
        &root,
        paired_auth(&paired.credential),
        None,
        true,
    );
    wait_connected(&fleet);
    poll_until(&fleet, "the outpost never reported its readiness", || {
        !outposts_list(&fleet)["outposts"][0]["readiness"].is_null()
    });
    let report = &outposts_list(&fleet)["outposts"][0]["readiness"];
    let ids: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert!(ids.contains(&"git"), "{report}");
    assert!(ids.contains(&"agent:no-such-agent"), "{report}");
    assert!(ids.contains(&"disk"), "{report}");

    let (status, refusal) = try_allocate(&fleet, "another-squad");
    assert_eq!(status, 409, "{refusal}");
    assert!(
        refusal["error"]
            .as_str()
            .unwrap()
            .contains("box is not ready"),
        "{refusal}"
    );
    assert!(
        refusal["remediation"]
            .as_str()
            .unwrap()
            .contains("[machine.agents]"),
        "{refusal}"
    );
    let saved = registry.load().unwrap();
    assert!(
        saved.machines["box"].readiness.is_some(),
        "the report is kept for `ralphus-fleet list`"
    );
}

fn root_of(dir: &std::path::Path) -> String {
    dir.to_string_lossy().replace('\\', "/")
}

#[test]
fn an_outpost_serves_an_allocations_machine_api_over_its_outbound_connection() {
    let dir = ralphus_test_support::TestDir::new("outpost-loopback");
    let root = root_of(&dir);
    let fleet = fleet(&root, SECRET);
    let _outpost = outpost(&fleet.url, &root, SECRET);
    wait_connected(&fleet);
    let (endpoint, token) = allocate(&fleet);

    let (status, ping) = call("POST", &format!("{endpoint}/ping"), &token, &json!({}));
    assert_eq!(status, 200);
    assert_eq!(ping["ok"], true, "{ping}");
    assert_eq!(ping["protocol_version"], 2);

    let path = format!("{root}/w/note.txt");
    let (_, wrote) = call(
        "POST",
        &format!("{endpoint}/write-file"),
        &token,
        &json!({"path": path, "content": "over the outpost"}),
    );
    assert_eq!(wrote["ok"], true, "{wrote}");
    let (_, read) = call(
        "POST",
        &format!("{endpoint}/read-file"),
        &token,
        &json!({"path": path}),
    );
    assert_eq!(read["stdout"], "over the outpost", "{read}");

    let (_, files) = call(
        "POST",
        &format!("{endpoint}/materialize"),
        &token,
        &json!({"source": path, "destination": "ignored"}),
    );
    assert_eq!(
        files["files"][0]["content_base64"], "b3ZlciB0aGUgb3V0cG9zdA==",
        "{files}"
    );

    let (_, caps) = call(
        "POST",
        &format!("{endpoint}/capabilities"),
        &token,
        &json!({}),
    );
    assert_eq!(caps["capabilities"]["os"], std::env::consts::OS, "{caps}");

    let (_, outside) = call(
        "POST",
        &format!("{endpoint}/read-file"),
        &token,
        &json!({"path": format!("{}/elsewhere", root_of(dir.parent().unwrap()))}),
    );
    assert_eq!(outside["ok"], false);
    assert!(
        outside["error"].as_str().unwrap().contains("outside"),
        "{outside}"
    );
}

#[test]
fn an_outpost_without_the_pairing_secret_is_never_handed_work() {
    let dir = ralphus_test_support::TestDir::new("outpost-wrong-secret");
    let root = root_of(&dir);
    let fleet = fleet(&root, SECRET);
    let _outpost = outpost(
        &fleet.url,
        &root,
        "an-entirely-different-secret-of-40-chars!",
    );
    std::thread::sleep(Duration::from_secs(2));
    assert!(!connected(&fleet), "a wrong secret must not connect");
    let (endpoint, token) = allocate(&fleet);
    let (_, ping) = call("POST", &format!("{endpoint}/ping"), &token, &json!({}));
    assert_eq!(ping["reason_code"], "unreachable", "{ping}");
}

#[test]
fn the_fleet_side_refuses_hellos_and_polls_it_cannot_authenticate() {
    let dir = ralphus_test_support::TestDir::new("outpost-hello-refused");
    let fleet = fleet(&root_of(&dir), SECRET);
    for hello in [
        json!({"protocol": "ralphus-outpost-v1", "name": "stranger", "nonce": "00".repeat(32)}),
        json!({"protocol": "ralphus-outpost-v1", "name": "box", "nonce": "short"}),
        json!({"protocol": "other", "name": "box", "nonce": "00".repeat(32)}),
    ] {
        let (status, _) = call(
            "POST",
            &format!("{}/v1/outpost/hello", fleet.url),
            "",
            &hello,
        );
        assert_eq!(status, 401, "{hello}");
    }
    let (status, granted) = call(
        "POST",
        &format!("{}/v1/outpost/hello", fleet.url),
        "",
        &json!({"protocol": "ralphus-outpost-v1", "name": "box", "nonce": "00".repeat(32)}),
    );
    assert_eq!(status, 200);
    let forged =
        json!({"session": granted["session"], "box": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"});
    let (status, _) = call(
        "POST",
        &format!("{}/v1/outpost/poll", fleet.url),
        "",
        &forged,
    );
    assert_eq!(status, 401, "a poll that does not open is refused");
    assert!(!connected(&fleet));
}

#[test]
fn a_request_for_another_remote_root_is_refused_by_the_outpost() {
    let dir = ralphus_test_support::TestDir::new("outpost-root-mismatch");
    let served = dir.join("served");
    let configured = dir.join("configured");
    let fleet = fleet(&root_of(&configured), SECRET);
    let _outpost = outpost(&fleet.url, &root_of(&served), SECRET);
    wait_connected(&fleet);
    let (endpoint, token) = allocate(&fleet);
    let (_, ping) = call("POST", &format!("{endpoint}/ping"), &token, &json!({}));
    assert_eq!(ping["ok"], false);
    assert!(
        ping["error"].as_str().unwrap().contains("--remote-root"),
        "{ping}"
    );
}
