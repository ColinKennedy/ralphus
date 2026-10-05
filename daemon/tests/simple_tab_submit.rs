//! Guards the board's "+ New Squad" Simple tab against schema drift: the TOML
//! its generator builds must pass the daemon's validator and be accepted by
//! `POST /api/squads`, the same endpoint the confirm step submits to.
//!
//! The generator is JavaScript (`librarian/assets/board/55-new-task-modal.js`),
//! so `test/emit-simple-task-toml.mjs` runs the real shipped `ntSimpleBuildToml`
//! under Node for a spread of form states -- proofs, each review mode, an agent
//! without system-prompt support -- and prints the documents as JSON. No model
//! is called: submission only enqueues the squad as Pending.
//!
//! Node is required; CI's runners ship it. Locally the test skips (loudly) when
//! `node` is missing, but fails if `CI` is set so the guard can't silently
//! vanish from the pipeline.

use std::path::PathBuf;
use std::process::Command;
use std::thread;

use ralphus_core::validate::validate_toml;
use ralphus_daemon::server::serve_with;
use ralphus_daemon::store::Store;

#[derive(serde::Deserialize)]
struct Emitted {
    name: String,
    toml: String,
}

fn emit() -> Option<Vec<Emitted>> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test")
        .join("emit-simple-task-toml.mjs");
    let output = match Command::new("node").arg(&script).output() {
        Ok(o) => o,
        Err(e) if std::env::var_os("CI").is_none() => {
            eprintln!("SKIPPED: `node` not runnable ({e}); Simple-tab submit guard not exercised");
            return None;
        }
        Err(e) => panic!("`node` must be available in CI to run the Simple-tab guard: {e}"),
    };
    assert!(
        output.status.success(),
        "emit-simple-task-toml.mjs failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(serde_json::from_slice(&output.stdout).expect("emitter prints a JSON array"))
}

fn send(base: &str, path: &str, body: &str) -> (u16, String) {
    let result = ureq::post(&format!("{base}{path}"))
        .set("Content-Type", "application/json")
        .set("X-Ralphus-User", "simple-tab-guard")
        .send_string(body);
    match result {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("request error: {e}"),
    }
}

#[test]
fn simple_tab_generated_toml_passes_the_validator() {
    let Some(cases) = emit() else { return };
    assert!(!cases.is_empty(), "emitter produced no cases");
    for case in &cases {
        let report = validate_toml(&case.toml);
        assert!(
            report.is_ok(),
            "Simple tab case {:?} generated TOML the validator rejects: {:?}\n--- toml ---\n{}",
            case.name,
            report.errors,
            case.toml
        );
    }
}

#[test]
fn simple_tab_generated_toml_is_accepted_by_submit() {
    let Some(cases) = emit() else { return };
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let store = Store::open_in_memory().expect("store");
    thread::spawn(move || serve_with(server, store, 2));
    let base = format!("http://{addr}");

    let user = serde_json::json!({ "name": "simple-tab-guard" }).to_string();
    let (status, text) = send(&base, "/api/users", &user);
    assert_eq!(status, 200, "register user: {text}");

    // The generator targets project "demo"; submit refuses unregistered ones.
    let repo = std::env::temp_dir().join(format!("ralphus-simple-tab-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let init = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .output()
        .expect("run git init");
    assert!(init.status.success(), "git init failed");
    let project = serde_json::json!({
        "name": "demo",
        "description": "",
        "path": repo.to_string_lossy().replace('\\', "/"),
        "vcs": "git",
    })
    .to_string();
    let (status, text) = send(&base, "/api/projects", &project);
    assert_eq!(status, 201, "register project: {text}");

    for case in &cases {
        let body = serde_json::json!({ "toml": case.toml, "label": case.name }).to_string();
        let (status, text) = send(&base, "/api/squads", &body);
        assert_eq!(
            status, 201,
            "submit rejected Simple tab case {:?}: {text}\n--- toml ---\n{}",
            case.name, case.toml
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}
