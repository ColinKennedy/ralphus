//! `ralphus initialize machine`: verify the local loopback provider.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::commands::initialize_exercise::Exercise;

pub fn dispatch(state_dir: Option<String>) -> i32 {
    let exercise = match Exercise::start("machine", state_dir) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let script = match loopback_script() {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let args = vec![script.to_string_lossy().into_owned()];
    if let Err(error) = exercise.client.register_machine(
        "loopback",
        "python",
        "Disposable local remote-machine exercise",
        Some(&args),
        None,
        false,
    ) {
        return fail(&format!("could not register loopback: {error}"));
    }
    match exercise.client.check_machine("loopback") {
        Ok(value) if value["ok"] == true => {}
        Ok(value) => return fail(&format!("loopback ping failed: {value}")),
        Err(error) => return fail(&format!("loopback ping failed: {error}")),
    }
    let (project, cwd) = match exercise.register_current_project("machine-exercise") {
        Ok(value) => value,
        Err(error) => return fail(&format!("could not register exercise project: {error}")),
    };
    let examples = match exercise.examples_dir() {
        Ok(value) => value,
        Err(error) => return fail(&format!("could not create examples directory: {error}")),
    };
    let toml = fixture_toml(&project, &cwd);
    if let Err(error) = std::fs::write(examples.join("loopback-exercise.toml"), &toml) {
        return fail(&error.to_string());
    }
    let squad = match exercise
        .client
        .submit(&toml, false, Some("loopback exercise: remote cell"))
        .map_err(|e| e.to_string())
        .and_then(squad_id)
    {
        Ok(value) => value,
        Err(error) => return fail(&format!("could not submit loopback fixture: {error}")),
    };
    if let Err(error) = wait_terminal(&exercise.client, &squad)
        .and_then(|_| wait_for_provider_state(&exercise.root.join("providers")))
    {
        return fail(&error);
    }
    println!("isolated loopback-machine exercise is ready.");
    println!("  daemon: {}", exercise.url);
    println!("  state: {}", exercise.root.display());
    println!(
        "  provider state: {}",
        exercise.root.join("providers").display()
    );
    println!("  completed remote squad: {squad}");
    println!(
        "Verified registration, ping, remote workspace provisioning, and remote raw-command execution."
    );
    0
}

fn fixture_toml(project: &str, cwd: &str) -> String {
    format!(
        "[[task]]\nname = \"loopback-exercise\"\nproject = {project:?}\nmachine = \"loopback:exercise\"\nno_commit_required = true\n\n[[task.cell]]\ncwd = {cwd:?}\ncommand = \"echo loopback machine exercise\"\nmode = \"raw\"\n"
    )
}

fn squad_id(value: Value) -> Result<String, String> {
    value["squad_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "submission response omitted squad_id".to_string())
}

fn wait_for_provider_state(root: &std::path::Path) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if root.join("ralphus-loopback-provider").exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("loopback fixture did not provision a private provider workspace".to_string())
}

fn wait_terminal(client: &crate::client::DaemonClient, squad: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let state = client.squad(squad).map_err(|e| e.to_string())?["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if matches!(state.as_str(), "done" | "failed" | "cancelled") {
            return if state == "done" {
                Ok(())
            } else {
                Err(format!("remote fixture ended {state}"))
            };
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("remote fixture did not reach a terminal state".to_string())
}

fn loopback_script() -> Result<PathBuf, String> {
    let path = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join("examples/providers/loopback.py");
    path.is_file().then_some(path).ok_or_else(|| {
        "examples/providers/loopback.py is unavailable from this checkout".to_string()
    })
}

fn fail(message: &str) -> i32 {
    println!("error: {message}");
    1
}
