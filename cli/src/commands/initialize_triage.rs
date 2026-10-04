//! `ralphus initialize triage`: deterministic pool registration exercise.

use std::{path::Path, process::Command};

use serde_json::Value;

use crate::commands::initialize_exercise::Exercise;

pub fn dispatch(state_dir: Option<String>) -> i32 {
    let exercise = match Exercise::start("triage", state_dir) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let repo = exercise.root.join("triage-fixture");
    if let Err(error) = create_tracking_repo(&repo) {
        return fail(&format!("could not create triage fixture: {error}"));
    }
    let project = format!("triage-exercise-{}", std::process::id());
    let cwd = repo.to_string_lossy().into_owned();
    if let Err(error) = exercise.client.register_project(
        &project,
        &cwd,
        "Disposable guided triage exercise",
        "git",
        None,
        false,
        None,
    ) {
        return fail(&format!("could not register exercise project: {error}"));
    }
    if let Err(error) = exercise.client.register_triage_type(
        "exercise",
        "Exercise",
        "A deterministic guided-exercise type.",
    ) {
        return fail(&format!("could not register type: {error}"));
    }
    if let Err(error) = exercise
        .client
        .set_triage_pool_threshold(&project, "exercise", Some(2))
    {
        return fail(&format!("could not set pool threshold: {error}"));
    }
    let examples = match exercise.examples_dir() {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let toml = fixture_toml(&project, &cwd);
    if let Err(error) = std::fs::write(examples.join("triage-exercise.toml"), &toml) {
        return fail(&error.to_string());
    }
    for n in 1..=2 {
        let _squad = match exercise
            .client
            // Triage enrollment happens at submission time. Holding these
            // candidates demonstrates pooling and threshold draining without
            // consuming an agent or leaving work running after the exercise.
            .submit(&toml, true, Some(&format!("triage exercise {n}")))
            .map_err(|e| e.to_string())
            .and_then(squad_id)
        {
            Ok(value) => value,
            Err(error) => return fail(&error),
        };
    }
    let drained = exercise
        .client
        .list_triage_pools()
        .map_err(|e| e.to_string())
        .ok()
        .and_then(|v| v["pools"].as_array().cloned())
        .is_some_and(|pools| {
            pools.iter().any(|pool| {
                pool["project"] == project
                    && pool["triage_type"] == "exercise"
                    && pool["count"] == 0
            })
        });
    if !drained {
        return fail("the two completed candidates did not drain the exercise triage pool");
    }
    println!("isolated triage exercise is ready.");
    println!("  daemon: {}", exercise.url);
    println!("  project/type: {project}/exercise; threshold: 2");
    println!(
        "  fixture: {}",
        examples.join("triage-exercise.toml").display()
    );
    println!(
        "submitted two raw candidates and verified the threshold drained the pool into a review."
    );
    0
}

fn fixture_toml(project: &str, cwd: &str) -> String {
    format!(
        "[[task]]\nname = \"triage-exercise\"\nproject = {project:?}\n\n[[task.cell]]\ncwd = {cwd:?}\ncommand = \"cmd /c exit 0\"\nmode = \"raw\"\ntriage = true\ntriage_type = \"exercise\"\n"
    )
}

fn squad_id(value: Value) -> Result<String, String> {
    value["squad_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "submission response omitted squad_id".to_string())
}

fn create_tracking_repo(repo: &Path) -> Result<(), String> {
    let origin = repo.with_file_name("triage-origin.git");
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
    std::fs::write(repo.join("README.md"), "# triage exercise\n").map_err(|e| e.to_string())?;
    run(repo, ["add", "README.md"])?;
    run(repo, ["commit", "--message", "seed triage exercise"])?;
    run(repo, ["remote", "add", "origin", &origin_text])?;
    run(repo, ["push", "--set-upstream", "origin", "main"])
}

fn run<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<(), String> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| e.to_string())?
        .success()
        .then_some(())
        .ok_or_else(|| "git fixture setup failed".to_string())
}

fn fail(message: &str) -> i32 {
    println!("error: {message}");
    1
}
