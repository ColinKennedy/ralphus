//! `ralphus-fleet` and `ralphus-outpost` over loopback: an outpost connects
//! out, the fleet hands it Machine API requests for an allocation, and the
//! replies come back -- and an outpost or a fleet without the right pairing
//! secret gets nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ralphus_fleet::allocations::Allocations;
use ralphus_fleet::config::FleetConfig;
use ralphus_fleet::server::{Service, serve_on};
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
    let options = ralphus_outpost::Options {
        fleet_url: url.to_string(),
        name: "box".to_string(),
        secret: secret.to_string(),
        remote_root: remote_root.to_string(),
        runner: vec!["ralphus-runner-not-needed".to_string()],
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

fn connected(fleet: &Fleet) -> bool {
    let (_, list) = call(
        "GET",
        &format!("{}/v1/outposts", fleet.url),
        SERVICE_TOKEN,
        &Value::Null,
    );
    list["outposts"][0]["connected"] == true
}

fn wait_connected(fleet: &Fleet) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !connected(fleet) {
        assert!(Instant::now() < deadline, "the outpost never connected");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn allocate(fleet: &Fleet) -> (String, String) {
    let (status, granted) = call(
        "POST",
        &format!("{}/v1/allocations", fleet.url),
        SERVICE_TOKEN,
        &json!({"requirements": {"labels": ["outpost"]}, "scope": {"squad_id": "s", "key": "k"}}),
    );
    assert_eq!(status, 200, "{granted}");
    (
        granted["endpoint"].as_str().unwrap().to_string(),
        granted["token"].as_str().unwrap().to_string(),
    )
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
