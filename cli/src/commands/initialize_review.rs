//! `ralphus initialize review`: disposable Git fixture for Guardian exercises.

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::commands::initialize_exercise::Exercise;

pub fn dispatch(state_dir: Option<String>) -> i32 {
    let exercise = match Exercise::start("review", state_dir) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let repo = exercise.root.join("review-fixture");
    if let Err(error) = create_repo(&repo) {
        return fail(&error);
    }
    let project = format!("review-exercise-{}", std::process::id());
    let repo_text = match std::env::current_dir() {
        Ok(cwd) => cwd.join(&repo).to_string_lossy().into_owned(),
        Err(error) => return fail(&format!("could not resolve fixture project path: {error}")),
    };
    if let Err(error) = exercise.client.register_project(
        &project,
        &repo_text,
        "Disposable Guardian review exercise",
        "git",
        None,
        false,
        None,
    ) {
        return fail(&format!("could not register fixture project: {error}"));
    }
    let examples = match exercise.examples_dir() {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let toml = fixture_toml(&project);
    if let Err(error) = std::fs::write(examples.join("review-exercise.toml"), &toml) {
        return fail(&error.to_string());
    }
    let squad = match exercise
        .client
        .submit(&toml, true, Some("review exercise: two held branches"))
        .map_err(|e| e.to_string())
        .and_then(squad_id)
    {
        Ok(value) => value,
        Err(error) => return fail(&format!("could not submit review fixture: {error}")),
    };
    let guardian = match wait_for_guardian(&exercise.client) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    println!("isolated review exercise is ready.");
    println!("  daemon: {}", exercise.url);
    println!("  fixture repository: {}", repo.display());
    println!(
        "  fixture TOML: {}",
        examples.join("review-exercise.toml").display()
    );
    println!("  held squad: {squad}");
    println!("  verified Guardian review count: {}", guardian.len());
    println!(
        "The two review branches are staged and held; activate the squad only when you want to run the raw fixture commands."
    );
    0
}

fn create_repo(repo: &Path) -> Result<(), String> {
    let origin = repo.with_file_name("review-origin.git");
    let origin_text = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join(&origin)
        .to_string_lossy()
        .into_owned();
    run(
        repo.parent().unwrap_or(repo),
        ["init", "--bare", &origin_text],
    )?;
    std::fs::create_dir_all(repo).map_err(|e| e.to_string())?;
    run(repo, ["init", "--initial-branch", "main"])?;
    run(repo, ["config", "user.email", "exercise@example.invalid"])?;
    run(repo, ["config", "user.name", "Ralphus Exercise"])?;
    std::fs::write(repo.join("README.md"), "# review exercise\n").map_err(|e| e.to_string())?;
    run(repo, ["add", "README.md"])?;
    run(repo, ["commit", "--message", "seed review exercise"])?;
    run(repo, ["remote", "add", "origin", &origin_text])?;
    run(repo, ["push", "--set-upstream", "origin", "main"])?;
    run(&origin, ["symbolic-ref", "HEAD", "refs/heads/main"])?;
    run(repo, ["remote", "set-head", "origin", "--auto"])
}

fn run<const N: usize>(repo: &Path, args: [&str; N]) -> Result<(), String> {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .map_err(|e| e.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "git fixture setup failed".to_string())
}

fn fixture_toml(project: &str) -> String {
    format!(
        "[[task]]\nname = \"first\"\nproject = {project:?}\n\n[[task.cell]]\ncwd = \"<<ralphus:new-worktree/review-first?upstream=<<default>>>>\"\ncommand = \"echo first branch\"\nmode = \"raw\"\nreview = \"<<ralphus:new-review/exercise>>\"\n\n[[task]]\nname = \"second\"\nproject = {project:?}\ndepends_on = [\"first\"]\n\n[[task.cell]]\ncwd = \"<<ralphus:new-worktree/review-second?upstream=<<default>>>>\"\ncommand = \"echo second branch\"\nmode = \"raw\"\nreview = \"<<ralphus:new-review/exercise>>\"\n\n[[review]]\nid = \"ralphus:new-review/exercise\"\nupstream = \"main\"\n"
    )
}

fn squad_id(value: Value) -> Result<String, String> {
    value["squad_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "submission response omitted squad_id".to_string())
}

fn wait_for_guardian(client: &crate::client::DaemonClient) -> Result<Vec<Value>, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match client.guardian_list().map_err(|e| e.to_string())? {
            Value::Array(guardians) if !guardians.is_empty() => return Ok(guardians),
            _ => thread::sleep(Duration::from_millis(100)),
        }
    }
    Err("submission did not create its Guardian review".to_string())
}
fn fail(message: &str) -> i32 {
    println!("error: {message}");
    1
}
