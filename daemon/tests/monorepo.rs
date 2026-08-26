//! Integration tests for monorepo (subproject-scoped cell) support (RAL-23).
//!
//! Tests at two levels:
//!
//! 1. **Pipeline tests** — always run. Use a `CapturingRunner` that records the
//!    `RunnerSpec` fed to it so we can assert the subproject system-prompt
//!    addendum is correctly injected through the full submit → schedule → run
//!    path.
//!
//! 2. **Live-runner test** — skips unless a `ralphus-runner` binary and an
//!    ollama server (with the resolver model pulled) are all locally reachable.
//!    Mirrors the pattern in `reviews_derive.rs`.  Run it with:
//!
//!    ```sh
//!    cargo test -p ralphus-daemon --test monorepo full_monorepo_flow -- --nocapture
//!    ```
//!
//!    Skip conditions: no runner found (`RALPHUS_RUNNER_CMD` or dev venv),
//!    pydantic-ai not installed in the runner's Python environment, ollama not
//!    up on `127.0.0.1:11434`, or resolver model (`RALPHUS_RESOLVER_MODEL`,
//!    default `qwen3:8b`) not pulled.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ralphus_core::schema::TaskFile;
use ralphus_daemon::guardian_merge::run_merge;
use ralphus_daemon::reviews::derive_reviews;
use ralphus_daemon::runner::{Runner, RunnerResult, RunnerSpec, SubprocessRunner};
use ralphus_daemon::scheduler::execute_squad;
use ralphus_daemon::store::{SquadState, Store};

// ── Helpers shared by both test levels ───────────────────────────────────────

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "ralphus")
        .env("GIT_AUTHOR_EMAIL", "ralphus@example.com")
        .env("GIT_COMMITTER_NAME", "ralphus")
        .env("GIT_COMMITTER_EMAIL", "ralphus@example.com")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn temp_base(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ralphus-mono-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Build a monorepo fixture: one git repository with `packages/alpha` and
/// `packages/beta` subdirectories, each containing a small file. Returns
/// the repo's absolute path (forward-slashed for TOML).
fn monorepo(base: &Path) -> String {
    let repo = base.join("repo");
    std::fs::create_dir_all(repo.join("packages/alpha")).unwrap();
    std::fs::create_dir_all(repo.join("packages/beta")).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("packages/alpha/mod.rs"), "// alpha\n").unwrap();
    std::fs::write(repo.join("packages/beta/mod.rs"), "// beta\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    repo.to_string_lossy().replace('\\', "/")
}

/// Like `monorepo` but also creates two linked worktrees that each modify a
/// shared file differently (to set up a merge conflict). Returns
/// `(repo_path, wt_a_path, wt_b_path)`, all forward-slashed.
fn monorepo_with_conflicting_worktrees(base: &Path) -> (String, String, String) {
    let repo = base.join("repo");
    std::fs::create_dir_all(repo.join("packages/alpha")).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("packages/alpha/lib.rs"), "// shared\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);

    let wt_a = base.join("wt-a");
    let wt_b = base.join("wt-b");
    git(
        &repo,
        &["worktree", "add", "-b", "feature/a", wt_a.to_str().unwrap()],
    );
    git(
        &repo,
        &["worktree", "add", "-b", "feature/b", wt_b.to_str().unwrap()],
    );
    std::fs::write(wt_a.join("packages/alpha/lib.rs"), "// changed by A\n").unwrap();
    git(&wt_a, &["commit", "-am", "a changes alpha"]);
    std::fs::write(wt_b.join("packages/alpha/lib.rs"), "// changed by B\n").unwrap();
    git(&wt_b, &["commit", "-am", "b changes alpha"]);
    // Set upstream so worktree_upstream() resolves to "main" during derive_reviews.
    git(&wt_a, &["branch", "--set-upstream-to=main", "feature/a"]);
    git(&wt_b, &["branch", "--set-upstream-to=main", "feature/b"]);

    (
        repo.to_string_lossy().replace('\\', "/"),
        wt_a.to_string_lossy().replace('\\', "/"),
        wt_b.to_string_lossy().replace('\\', "/"),
    )
}

// ── CapturingRunner ──────────────────────────────────────────────────────────

/// A test runner that immediately marks every cell done and records each
/// `RunnerSpec` it receives. Lets us assert that the system-prompt addendum
/// was injected without spawning a real subprocess.
#[derive(Clone, Default)]
struct CapturingRunner {
    specs: Arc<Mutex<Vec<RunnerSpec>>>,
}

impl CapturingRunner {
    fn captured(&self) -> Vec<RunnerSpec> {
        self.specs.lock().unwrap().clone()
    }
}

impl Runner for CapturingRunner {
    fn run(&self, spec: &RunnerSpec) -> RunnerResult {
        self.specs.lock().unwrap().push(spec.clone());
        RunnerResult {
            status: "done".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            summary: "captured".to_string(),
            error: None,
            proofed: None,
            agent_session_id: None,
            ghost: None,
        }
    }
}

// ── Pipeline-level tests (always run) ────────────────────────────────────────

/// A cell with `subprojects` set must round-trip through submit → store →
/// schedule → run with the subprojects system-prompt addendum present in the
/// spec delivered to the runner.
#[test]
fn subprojects_system_prompt_injected_through_full_pipeline() {
    let base = temp_base("inject");
    let repo = monorepo(&base);

    let toml = format!(
        "[[task]]\nname=\"t\"\n\
         [[task.cell]]\ncwd=\"{repo}\"\ncommand=\"echo ok\"\nsubprojects=[\"packages/alpha\"]\n"
    );
    assert!(
        ralphus_core::validate::validate_toml(&toml).is_ok(),
        "ticket TOML must validate: {:?}",
        ralphus_core::validate::validate_toml(&toml).errors
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let squad_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_squad(&store, &runner, &squad_id);

    assert_eq!(
        store.lock().unwrap().squad_state(&squad_id).unwrap(),
        SquadState::Done,
        "squad must complete"
    );

    let specs = runner.captured();
    assert_eq!(specs.len(), 1, "exactly one cell dispatched");
    let sp = specs[0]
        .system_prompt
        .as_deref()
        .expect("system_prompt must be injected when subprojects is set");
    assert!(
        sp.contains("packages/alpha"),
        "addendum must name the subproject: {sp}"
    );
    assert!(
        sp.contains("monorepo"),
        "addendum must mention monorepo: {sp}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// Two cells in one task each declare different subprojects. Both must
/// receive independent addenda naming their own package.
#[test]
fn two_subprojects_cells_get_independent_addenda() {
    let base = temp_base("two");
    let repo = monorepo(&base);

    let toml = format!(
        "[[task]]\nname=\"t\"\n\
         [[task.cell]]\ncwd=\"{repo}\"\ncommand=\"echo alpha\"\nsubprojects=[\"packages/alpha\"]\n\
         [[task.cell]]\ncwd=\"{repo}\"\ncommand=\"echo beta\"\nsubprojects=[\"packages/beta\"]\n"
    );
    assert!(
        ralphus_core::validate::validate_toml(&toml).is_ok(),
        "{:?}",
        ralphus_core::validate::validate_toml(&toml).errors
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let squad_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_squad(&store, &runner, &squad_id);

    assert_eq!(
        store.lock().unwrap().squad_state(&squad_id).unwrap(),
        SquadState::Done
    );

    let specs = runner.captured();
    assert_eq!(specs.len(), 2);
    let has_alpha = specs.iter().any(|s| {
        s.system_prompt
            .as_deref()
            .is_some_and(|sp| sp.contains("packages/alpha"))
    });
    let has_beta = specs.iter().any(|s| {
        s.system_prompt
            .as_deref()
            .is_some_and(|sp| sp.contains("packages/beta"))
    });
    assert!(has_alpha, "alpha cell must carry the alpha addendum");
    assert!(has_beta, "beta cell must carry the beta addendum");

    let _ = std::fs::remove_dir_all(&base);
}

/// A cell without `subprojects` must not have a system-prompt injected.
#[test]
fn no_subprojects_no_injection() {
    let base = temp_base("none");
    let repo = monorepo(&base);

    let toml = format!(
        "[[task]]\nname=\"t\"\n\
         [[task.cell]]\ncwd=\"{repo}\"\ncommand=\"echo ok\"\n"
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let squad_id = store
        .lock()
        .unwrap()
        .insert_squad(&file, None, false)
        .unwrap();

    let runner = CapturingRunner::default();
    execute_squad(&store, &runner, &squad_id);

    let specs = runner.captured();
    assert_eq!(specs.len(), 1);
    assert!(
        specs[0].system_prompt.is_none(),
        "no system_prompt should be injected when subprojects is absent"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ── Live-runner test (requires ollama + ralphus-runner) ──────────────────────

fn ollama_up() -> bool {
    use std::net::TcpStream;
    "127.0.0.1:11434"
        .parse()
        .ok()
        .and_then(|addr| {
            TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(400)).ok()
        })
        .is_some()
}

fn ollama_has_model(model: &str) -> bool {
    match ureq::get("http://127.0.0.1:11434/api/tags").call() {
        Ok(resp) => resp
            .into_string()
            .map(|b| b.contains(model))
            .unwrap_or(false),
        Err(_) => false,
    }
}

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

fn pydantic_ai_available(runner_cmd: &str) -> bool {
    let runner_path = Path::new(runner_cmd);
    let Some(dir) = runner_path.parent() else {
        return false;
    };
    for python in ["python.exe", "python3.exe", "python", "python3"] {
        let p = dir.join(python);
        if p.exists() {
            return Command::new(p)
                .args(["-c", "import pydantic_ai"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        }
    }
    false
}

/// Full monorepo flow: two subproject-scoped cells in one monorepo repo
/// → a review with a merge conflict between their branches → live ollama
/// agent resolves the conflict. Skips unless ollama + a model + ralphus-runner
/// are all reachable locally.
///
/// Run with:
///   cargo test -p ralphus-daemon --test monorepo full_monorepo_flow -- --ignored --nocapture
#[test]
#[ignore = "calls a live local Ollama model; run explicitly with `cargo test -- --ignored`"]
fn full_monorepo_flow_with_subproject_cells() {
    let Some(runner_cmd) = find_runner() else {
        eprintln!("SKIP full_monorepo_flow: ralphus-runner not found (set RALPHUS_RUNNER_CMD)");
        return;
    };
    if !pydantic_ai_available(&runner_cmd) {
        eprintln!(
            "SKIP full_monorepo_flow: pydantic-ai not installed in runner environment (run `uv sync --extra runner` in cli/)"
        );
        return;
    }
    if !ollama_up() {
        eprintln!("SKIP full_monorepo_flow: ollama not reachable on 127.0.0.1:11434");
        return;
    }
    let model = std::env::var("RALPHUS_RESOLVER_MODEL").unwrap_or_else(|_| "qwen3:8b".to_string());
    if !ollama_has_model(&model) {
        eprintln!("SKIP full_monorepo_flow: ollama model '{model}' not pulled");
        return;
    }

    let base = temp_base("live");
    let (_repo, cwd_a, cwd_b) = monorepo_with_conflicting_worktrees(&base);

    // 1) Build the ticket TOML: two tasks, each a command cell in a worktree
    //    of the same monorepo, both scoped to `packages/alpha`.
    let toml = format!(
        "[[task]]\nname=\"a\"\n\
         [[task.cell]]\ncwd=\"{cwd_a}\"\ncommand=\"echo a-done\"\nsubprojects=[\"packages/alpha\"]\nreview=\"<<review:rev>>\"\n\
         [[task]]\nname=\"b\"\ndepends_on=[\"a\"]\n\
         [[task.cell]]\ncwd=\"{cwd_b}\"\ncommand=\"echo b-done\"\nsubprojects=[\"packages/alpha\"]\nreview=\"<<review:rev>>\"\n\
         [[review]]\nid=\"rev\"\n"
    );
    assert!(
        ralphus_core::validate::validate_toml(&toml).is_ok(),
        "ticket TOML must validate: {:?}",
        ralphus_core::validate::validate_toml(&toml).errors
    );
    let file: TaskFile = toml::from_str(&toml).unwrap();

    // 2) Submit and run the tasks. Cells are command-only, so they succeed
    //    instantly without a real model.
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let squad_id = {
        let mut g = store.lock().unwrap();
        g.insert_squad(&file, Some("monorepo-ticket"), false)
            .unwrap()
    };
    let ok_runner = SubprocessRunner::new(&runner_cmd);
    execute_squad(&store, &ok_runner, &squad_id);
    assert_eq!(
        store.lock().unwrap().squad_state(&squad_id).unwrap(),
        SquadState::Done,
        "both cells must succeed"
    );

    // 3) Derive the review: both worktrees are one project (same git root) →
    //    one guardian with two branches.
    let ids = {
        let g = store.lock().unwrap();
        derive_reviews(&g, &squad_id, &file).expect("derive ok")
    };
    assert_eq!(ids.len(), 1, "one git root → one review");
    let gid = ids[0].clone();
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get_guardian(&gid)
            .unwrap()
            .branches
            .len(),
        2,
        "two branches in the review"
    );

    // 4) Build the review. feature/b conflicts with feature/a on
    //    packages/alpha/lib.rs; the live ollama agent must resolve it.
    run_merge(&store, &ok_runner, &gid);

    let view = store.lock().unwrap().get_guardian(&gid).unwrap();
    assert_eq!(view.status, "in_review", "detail: {:?}", view.detail);

    let combined = view
        .combined_worktree
        .expect("combined worktree after merge");
    let merged =
        std::fs::read_to_string(Path::new(&combined).join("packages/alpha/lib.rs")).unwrap();
    assert!(
        !merged.contains("<<<<<<<"),
        "conflict markers must be resolved: {merged}"
    );

    let _ = std::fs::remove_dir_all(&base);
}
