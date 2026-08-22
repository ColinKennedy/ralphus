//! Real end-to-end integration test for `prompt`-kind proof execution.
//!
//! A Task TOML with an `[[task.proof]]` `prompt` step is validated, submitted,
//! and run through the *real* `ralphus-runner` subprocess against a live local
//! Ollama model — the same store/scheduler/runner path production code takes,
//! with no fakes. The proof step's final state is asserted from the store.
//!
//! Skips (prints `SKIP`, does not fail) unless ollama is up on
//! `127.0.0.1:11434`, the model (`RALPHUS_VERIFY_MODEL`, default `qwen3:8b`)
//! is pulled, and a `ralphus-runner` is found (`RALPHUS_RUNNER_CMD` or the dev
//! venv) — same idiom as `reviews_derive.rs`'s `full_flow` test.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use ralphus_core::schema::TaskFile;
use ralphus_daemon::runner::SubprocessRunner;
use ralphus_daemon::scheduler::execute_squad;
use ralphus_daemon::store::{SquadState, Store};

/// Both tests below build a fresh in-memory `Store` and therefore get the
/// same deterministic squad/task/cell ids, which collapse to the same tmux
/// session name (`session_name` only keys on those ids). Run them one at a
/// time so two "new-session" calls for that identical name can't race.
fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Whether an ollama server is listening on the default port.
fn ollama_up() -> bool {
    use std::net::TcpStream;
    use std::time::Duration;
    "127.0.0.1:11434"
        .parse()
        .ok()
        .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(400)).ok())
        .is_some()
}

/// Whether ollama has `model` pulled (checked via its /api/tags).
fn ollama_has_model(model: &str) -> bool {
    match ureq::get("http://127.0.0.1:11434/api/tags").call() {
        Ok(resp) => resp
            .into_string()
            .map(|b| b.contains(model))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Locate a `ralphus-runner` to drive: `RALPHUS_RUNNER_CMD`, else the dev venv.
fn find_runner() -> Option<String> {
    if let Ok(cmd) = std::env::var("RALPHUS_RUNNER_CMD") {
        if !cmd.trim().is_empty() {
            return Some(cmd);
        }
    }
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    for rel in [
        "cli/.venv/Scripts/ralphus-runner.exe",
        "cli/.venv/bin/ralphus-runner",
    ] {
        let p = ws.join(rel);
        if p.exists() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

/// A fresh temp directory to use as a session's `cwd`.
fn temp_base(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ralphus-prompt-verify-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Build, validate, submit, and run a one-task TOML whose task-level proof is
/// a `prompt` step with the given claim, returning the proof step's state and
/// output. The cell itself is a deterministic `command` (no model needed);
/// its `agent`/`model` are only there to be inherited by the proof step.
fn run_one_claim(model: &str, claim: &str, tag: &str) -> (String, Option<String>, SquadState) {
    let runner_cmd = find_runner().expect("checked by caller");
    let base = temp_base(tag);
    let cwd = base.to_string_lossy().replace('\\', "/");

    // The task name feeds `tmux::session_name(squad_id, task, cell_id)`, and
    // `squad_id` is deterministic per fresh in-memory Store (always
    // "squad-000000000001"). Since cargo runs tests in the same binary
    // concurrently by default, two tests both named "t" would race on the
    // *same* tmux session name — one test's setup can kill/overwrite the
    // other's live session mid-run, so a test can read back its sibling's
    // pane output instead of its own. `tag` keeps the two tests' sessions
    // distinct.
    let toml = format!(
        "[[task]]\nname=\"t-{tag}\"\n\
         [[task.cell]]\ncwd=\"{cwd}\"\ncommand=\"echo noop\"\nagent=\"ollama\"\nmodel=\"{model}\"\n\
         [[task.proof]]\nid=\"claim\"\nprompt=\"Confirm this arithmetic claim: {claim}\"\n"
    );
    assert!(
        ralphus_core::validate::validate_toml(&toml).is_ok(),
        "proof TOML must validate: {:?}",
        ralphus_core::validate::validate_toml(&toml).errors
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let squad_id = {
        let mut g = store.lock().unwrap();
        g.insert_squad(&file, Some("prompt-verify-test"), false)
            .unwrap()
    };

    let runner = SubprocessRunner::new(&runner_cmd);
    execute_squad(&store, &runner, &squad_id);

    let guard = store.lock().unwrap();
    let squad_state = guard.squad_state(&squad_id).unwrap();
    let squad = guard.get_squad(&squad_id).unwrap();
    let proof = &squad.tasks[0].proof[0];
    let result = (proof.kind.clone(), proof.output.clone(), squad_state);

    let _ = std::fs::remove_dir_all(&base);
    result
}

#[test]
#[ignore = "calls a live local Ollama model; run explicitly with `cargo test -- --ignored`"]
fn prompt_verify_passes_a_true_claim_via_real_ollama() {
    if find_runner().is_none() {
        eprintln!("SKIP prompt_verify: ralphus-runner not found (set RALPHUS_RUNNER_CMD)");
        return;
    }
    if !ollama_up() {
        eprintln!("SKIP prompt_verify: ollama not reachable on 127.0.0.1:11434");
        return;
    }
    let model = std::env::var("RALPHUS_VERIFY_MODEL").unwrap_or_else(|_| "qwen3:8b".to_string());
    if !ollama_has_model(&model) {
        eprintln!("SKIP prompt_verify: ollama model '{model}' not pulled");
        return;
    }

    let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let (kind, output, squad_state) = run_one_claim(&model, "2 + 2 = 4", "true");
    assert_eq!(kind, "prompt");
    assert_eq!(squad_state, SquadState::Done, "proof output: {output:?}");
}

#[test]
#[ignore = "calls a live local Ollama model; run explicitly with `cargo test -- --ignored`"]
fn prompt_verify_fails_a_false_claim_via_real_ollama() {
    if find_runner().is_none() {
        eprintln!("SKIP prompt_verify: ralphus-runner not found (set RALPHUS_RUNNER_CMD)");
        return;
    }
    if !ollama_up() {
        eprintln!("SKIP prompt_verify: ollama not reachable on 127.0.0.1:11434");
        return;
    }
    let model = std::env::var("RALPHUS_VERIFY_MODEL").unwrap_or_else(|_| "qwen3:8b".to_string());
    if !ollama_has_model(&model) {
        eprintln!("SKIP prompt_verify: ollama model '{model}' not pulled");
        return;
    }

    let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let (kind, output, squad_state) = run_one_claim(&model, "2 + 2 = 5", "false");
    assert_eq!(kind, "prompt");
    assert_eq!(squad_state, SquadState::Failed, "proof output: {output:?}");
}
