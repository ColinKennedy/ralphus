//! `ralphus initialize review`: disposable Git fixture for Guardian exercises.
//!
//! Locally, two raw branches are staged and held so the reviewer can activate
//! them when ready. With `--remote`, both branches run on the strict loopback
//! machine, commit and push their own files, and the review merges on that
//! machine through to `in_review`.

use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::commands::initialize_exercise::{
    Exercise, ExerciseOptions, REMOTE_MACHINE, commit_and_push_command, squad_id,
};

pub fn dispatch(options: &ExerciseOptions) -> i32 {
    let exercise = match Exercise::start("review", options) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let fixture = match exercise.fixture_project("review") {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let examples = match exercise.examples_dir() {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let toml = fixture_toml(&fixture.name, exercise.remote());
    if let Err(error) = std::fs::write(examples.join("review-exercise.toml"), &toml) {
        return fail(&error.to_string());
    }
    let hold = !exercise.remote();
    let squad = match exercise
        .client
        .submit(&toml, hold, Some("review exercise: two branches"))
        .map_err(|e| e.to_string())
        .and_then(|v| squad_id(&v))
    {
        Ok(value) => value,
        Err(error) => return fail(&format!("could not submit review fixture: {error}")),
    };
    let guardian = match wait_for_guardian(&exercise.client) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    if exercise.remote() {
        match exercise.wait_terminal(&squad, Duration::from_secs(180)) {
            Ok(state) if state == "done" => {}
            Ok(state) => return fail(&format!("review squad {squad} ended {state}")),
            Err(error) => return fail(&error),
        }
        let id = guardian["id"].as_str().unwrap_or_default().to_string();
        if let Err(error) = wait_in_review(&exercise.client, &id) {
            return fail(&error);
        }
    }
    println!("isolated review exercise is ready.");
    println!("  daemon: {}", exercise.url);
    println!("  fixture repository: {}", fixture.path.display());
    println!(
        "  fixture TOML: {}",
        examples.join("review-exercise.toml").display()
    );
    if exercise.remote() {
        println!("  machine: {REMOTE_MACHINE} (strict loopback)");
        println!("  completed squad: {squad}");
        println!(
            "Both branches were committed and pushed on the machine, and the review merged there into in_review."
        );
    } else {
        println!("  held squad: {squad}");
        println!(
            "The two review branches are staged and held; activate the squad only when you want to run the raw fixture commands."
        );
    }
    0
}

/// Two raw review branches on `project`. Remote: both run on
/// [`REMOTE_MACHINE`], commit and push their own file, and the review merges
/// there. Local: plain echoes, submitted held.
fn fixture_toml(project: &str, remote: bool) -> String {
    let machine = if remote {
        format!("machine = {REMOTE_MACHINE:?}\n")
    } else {
        String::new()
    };
    let (first, second) = if remote {
        (
            commit_and_push_command("first.txt", "first"),
            commit_and_push_command("second.txt", "second"),
        )
    } else {
        (
            "echo first branch".to_string(),
            "echo second branch".to_string(),
        )
    };
    // A review branch is always a fresh worktree, local or remote.
    let first_cwd = "<<ralphus:new-worktree/review-first?upstream=<<default>>>>";
    let second_cwd = "<<ralphus:new-worktree/review-second?upstream=<<default>>>>";
    let review_machine = &machine;
    format!(
        "[[task]]\nname = \"first\"\nproject = {project:?}\n{machine}\n[[task.cell]]\ncwd = {first_cwd:?}\ncommand = {first:?}\nmode = \"raw\"\nreview = \"<<ralphus:new-review/exercise>>\"\n\n[[task]]\nname = \"second\"\nproject = {project:?}\n{machine}depends_on = [\"first\"]\n\n[[task.cell]]\ncwd = {second_cwd:?}\ncommand = {second:?}\nmode = \"raw\"\nreview = \"<<ralphus:new-review/exercise>>\"\n\n[[review]]\nid = \"ralphus:new-review/exercise\"\nupstream = \"main\"\n{review_machine}proof_scope = \"nothing\"\nskip_manual_checks = true\n"
    )
}

fn wait_for_guardian(client: &crate::client::DaemonClient) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match client.guardian_list().map_err(|e| e.to_string())? {
            Value::Array(guardians) if !guardians.is_empty() => {
                return Ok(guardians[0].clone());
            }
            _ => thread::sleep(Duration::from_millis(100)),
        }
    }
    Err("submission did not create its Guardian review".to_string())
}

fn wait_in_review(client: &crate::client::DaemonClient, id: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut last = String::new();
    while Instant::now() < deadline {
        let review = client.guardian_get(id).map_err(|e| e.to_string())?;
        last = review["status"].as_str().unwrap_or_default().to_string();
        match last.as_str() {
            "in_review" => return Ok(()),
            "merge_failed" | "cancelled" => {
                return Err(format!(
                    "review {id} ended {last}: {}",
                    review["detail"].as_str().unwrap_or("no detail")
                ));
            }
            _ => thread::sleep(Duration::from_millis(500)),
        }
    }
    Err(format!(
        "review {id} did not reach in_review (last status {last})"
    ))
}

fn fail(message: &str) -> i32 {
    println!("error: {message}");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_remote_fixtures_are_valid_task_files() {
        for remote in [false, true] {
            let toml = fixture_toml("p", remote);
            let report = ralphus_core::validate::validate_toml(&toml);
            assert!(
                report.errors.is_empty(),
                "remote={remote}: {:?}",
                report.errors
            );
            let file: ralphus_core::schema::TaskFile = toml::from_str(&toml).unwrap();
            let expected = remote.then_some(REMOTE_MACHINE);
            assert!(file.task.iter().all(|t| t.machine.as_deref() == expected));
            assert_eq!(file.review[0].machine.as_deref(), expected);
            assert_eq!(file.review[0].proof_scope.as_deref(), Some("nothing"));
        }
    }
}
