//! End-to-end integration tests: spawn the actual compiled `ralphus` binary
//! against a real (throwaway, local) fake daemon server and assert on its
//! stdout/exit code -- unlike the unit tests in `src/`, which call Rust
//! functions directly, this exercises the whole pipeline (argv parsing,
//! `--daemon-url`/`--json` extraction, HTTP call, rendering) the way a real
//! invocation does. Mirrors the spirit of the Python suite's
//! `test_cli_commands.py` (exercise the real CLI end-to-end) without
//! attempting a line-for-line port of its ~1,129 lines.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

struct FakeDaemon {
    server: Arc<tiny_http::Server>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeDaemon {
    /// Serves every request received during the test with the same
    /// `(status, body)`, until the server is dropped. Some commands
    /// legitimately make more than one request per invocation (e.g. `get`
    /// re-fetches the run after `resolve_run_selector` already fetched it
    /// internally -- a redundancy this port faithfully carries over from
    /// the Python original's own `_cmd_get`), so a single-shot responder
    /// would hang those tests waiting for a second reply that never comes.
    fn once(status: u16, body: &'static str) -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let for_thread = Arc::clone(&server);
        let handle = std::thread::spawn(move || {
            while let Ok(Some(request)) = for_thread.recv_timeout(Duration::from_secs(5)) {
                let response = tiny_http::Response::from_string(body).with_status_code(status);
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

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run_cli(daemon_url: &str, args: &[&str]) -> (i32, String) {
    let exe = env!("CARGO_BIN_EXE_ralphus");
    let output = Command::new(exe)
        .arg("--daemon-url")
        .arg(daemon_url)
        .args(args)
        .output()
        .expect("failed to spawn ralphus binary");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    (output.status.code().unwrap_or(-1), stdout)
}

#[test]
fn status_prints_run_list_table() {
    let daemon = FakeDaemon::once(
        200,
        r#"{"runs":[{"id":"run-1","state":"done","label":"my run"}]}"#,
    );
    let (code, stdout) = run_cli(&daemon.url(), &["status"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("run-1"));
    assert!(stdout.contains("done"));
    assert!(stdout.contains("my run"));
}

#[test]
fn status_json_mode_emits_raw_json() {
    let daemon = FakeDaemon::once(200, r#"{"runs":[]}"#);
    let (code, stdout) = run_cli(&daemon.url(), &["--json", "status"]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON output");
    assert_eq!(parsed["runs"].as_array().unwrap().len(), 0);
}

#[test]
fn unreachable_daemon_exits_1_with_hint() {
    // Nothing listens on this port.
    let (code, stdout) = run_cli("http://127.0.0.1:1", &["status"]);
    assert_eq!(code, 1);
    assert!(stdout.contains("is the daemon running"));
}

#[test]
fn not_found_maps_to_exit_code_3() {
    let daemon = FakeDaemon::once(
        404,
        r#"{"error":{"code":"not_found","message":"no such run"}}"#,
    );
    let (code, stdout) = run_cli(&daemon.url(), &["run", "show", "missing-run"]);
    assert_eq!(code, 3);
    assert!(stdout.contains("no such run"));
}

#[test]
fn agent_list_needs_no_daemon_at_all() {
    // `agent list` is purely static -- no HTTP call at all -- so a garbage
    // daemon URL must not matter.
    let (code, stdout) = run_cli("http://127.0.0.1:1", &["agent", "list"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("claude-code"));
    assert!(stdout.contains("codex"));
}

#[test]
fn validate_reports_missing_file_as_usage_style_error_not_panic() {
    let daemon = FakeDaemon::once(200, r#"{"valid":true,"errors":[],"warnings":[]}"#);
    let (code, stdout) = run_cli(
        &daemon.url(),
        &["validate", "definitely-does-not-exist.toml"],
    );
    assert_eq!(code, 1);
    assert!(stdout.contains("could not read"));
}

#[test]
fn unknown_subcommand_is_a_usage_error_not_a_panic() {
    let (code, stdout) = run_cli("http://127.0.0.1:1", &["totally-bogus-command"]);
    assert_eq!(code, 2);
    assert!(stdout.contains("usage error"));
}

#[test]
fn quick_start_with_no_args_prints_help_rather_than_launching_anything() {
    let (code, stdout) = run_cli("http://127.0.0.1:1", &["quick-start"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("quick-start"));
}

#[test]
fn quick_start_unknown_subcommand_is_a_usage_error() {
    let (code, stdout) = run_cli("http://127.0.0.1:1", &["quick-start", "bogus"]);
    assert_eq!(code, 2);
    assert!(stdout.contains("usage error"));
}

#[test]
fn get_walks_dotted_field_path() {
    let daemon = FakeDaemon::once(
        200,
        r#"{"id":"run-1","label":"","tasks":[{"name":"build","state":"done","verify":[],"sessions":[]}]}"#,
    );
    let (code, stdout) = run_cli(&daemon.url(), &["get", "run-1/0", "state"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "done");
}

/// A single request handler that reads and can assert on the request body --
/// used for the one test below that needs to check what the CLI actually
/// sent, not just what it printed back.
fn serve_and_capture_body(
    status: u16,
    response_body: &'static str,
) -> (FakeDaemon, std::sync::mpsc::Receiver<String>) {
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let for_thread = Arc::clone(&server);
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        if let Ok(Some(mut request)) = for_thread.recv_timeout(Duration::from_secs(10)) {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let _ = tx.send(body);
            let response = tiny_http::Response::from_string(response_body).with_status_code(status);
            let _ = request.respond(response);
        }
    });
    (
        FakeDaemon {
            server,
            handle: Some(handle),
        },
        rx,
    )
}

#[test]
fn submit_reads_file_and_posts_its_contents() {
    let (daemon, rx) = serve_and_capture_body(200, r#"{"run_id":"run-1","state":"pending"}"#);
    let dir = std::env::temp_dir().join(format!("ralphus-cli-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let toml_path = dir.join("task.toml");
    std::fs::write(&toml_path, "[[task]]\nname=\"t\"\n").unwrap();

    let (code, stdout) = run_cli(
        &daemon.url(),
        &["submit", "--no-validate", toml_path.to_str().unwrap()],
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("run-1"));

    let body = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("daemon received a request");
    assert!(body.contains("name=\\\"t\\\"") || body.contains("[[task]]"));

    std::fs::remove_dir_all(&dir).ok();
}
