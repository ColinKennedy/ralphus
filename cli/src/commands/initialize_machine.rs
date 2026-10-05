//! `ralphus initialize machine`: verify the local loopback provider.
//!
//! Always runs against the strict loopback machine (see
//! `initialize_exercise::REMOTE_MACHINE`), with or without `--remote`: the
//! machine *is* the subject of this exercise. One task provisions a fresh
//! worktree on the machine, runs a cell there with an environment variable,
//! and proves on the machine that the variable arrived.

use std::time::Duration;

use crate::commands::initialize_exercise::{Exercise, ExerciseOptions, REMOTE_MACHINE, squad_id};

const EXPECTED: &str = "remote-ok";

pub fn dispatch(options: &ExerciseOptions) -> i32 {
    let options = ExerciseOptions {
        remote: true,
        ..options.clone()
    };
    let exercise = match Exercise::start("machine", &options) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let fixture = match exercise.fixture_project("machine") {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let examples = match exercise.examples_dir() {
        Ok(value) => value,
        Err(error) => return fail(&format!("could not create examples directory: {error}")),
    };
    let toml = fixture_toml(
        &fixture.name,
        &exercise.cell_cwd(&fixture, "machine-exercise"),
    );
    if let Err(error) = std::fs::write(examples.join("loopback-exercise.toml"), &toml) {
        return fail(&error.to_string());
    }
    let squad = match exercise
        .client
        .submit(&toml, false, Some("loopback exercise: remote cell"))
        .map_err(|e| e.to_string())
        .and_then(|v| squad_id(&v))
    {
        Ok(value) => value,
        Err(error) => return fail(&format!("could not submit loopback fixture: {error}")),
    };
    match exercise.wait_terminal(&squad, Duration::from_secs(120)) {
        Ok(state) if state == "done" => {}
        Ok(state) => return fail(&format!("remote fixture {squad} ended {state}")),
        Err(error) => return fail(&error),
    }
    let remote_root = exercise.root.join("remote-root");
    let cwd = match exercise.client.squad(&squad) {
        Ok(value) => value["tasks"][0]["cells"][0]["cwd"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        Err(error) => return fail(&error.to_string()),
    };
    if !std::path::Path::new(&cwd).starts_with(&remote_root) {
        return fail(&format!(
            "the cell ran in {cwd:?}, not in a workspace provisioned under {}",
            remote_root.display()
        ));
    }
    println!("isolated loopback-machine exercise is ready.");
    println!("  daemon: {}", exercise.url);
    println!("  state: {}", exercise.root.display());
    println!("  machine: {REMOTE_MACHINE} (strict loopback)");
    println!("  provisioned workspace: {cwd}");
    println!("  completed remote squad: {squad}");
    println!(
        "Verified registration, ping, workspace provisioning under remote_root, remote command execution, environment propagation, and a remote proof."
    );
    0
}

/// One remote task: the cell writes its environment variable into a file and
/// the proof (also on the machine) checks it. `cmd` on Windows, `sh`
/// elsewhere -- the exercise daemon pins `RALPHUS_SHELL` to match.
fn fixture_toml(project: &str, cwd: &str) -> String {
    let (write, check) = if cfg!(windows) {
        (
            "echo %EXERCISE_ENV%> machine-exercise.txt",
            format!("findstr /x {EXPECTED} machine-exercise.txt"),
        )
    } else {
        (
            "echo \"$EXERCISE_ENV\" > machine-exercise.txt",
            format!("grep -qx {EXPECTED} machine-exercise.txt"),
        )
    };
    format!(
        "[[task]]\nname = \"loopback-exercise\"\nproject = {project:?}\nmachine = {REMOTE_MACHINE:?}\nno_commit_required = true\n\n[[task.cell]]\ncwd = {cwd:?}\ncommand = {write:?}\nmode = \"raw\"\n\n[task.cell.environment]\nEXERCISE_ENV = {EXPECTED:?}\n\n[[task.cell.proof]]\ncommand = {check:?}\nmode = \"raw\"\n"
    )
}

fn fail(message: &str) -> i32 {
    println!("error: {message}");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_is_a_valid_remote_task_file() {
        let toml = fixture_toml("p", "<<ralphus:new-worktree/b?upstream=<<default>>>>");
        let file: ralphus_core::schema::TaskFile = toml::from_str(&toml).unwrap();
        assert_eq!(file.task[0].machine.as_deref(), Some(REMOTE_MACHINE));
        assert_eq!(file.task[0].cell[0].environment["EXERCISE_ENV"], EXPECTED);
        let report = ralphus_core::validate::validate_toml(&toml);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }
}
