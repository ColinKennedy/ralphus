//! `ralphus initialize waypoint`: a disposable, live waypoint exercise.
//!
//! The command owns a separate daemon process, SQLite database, token, and
//! global configuration directory. Held examples exercise enrollment and
//! guidance without starting agents; the completed fixture uses one raw
//! command to prove the explicit redo path.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::client::DaemonClient;
use crate::commands::initialize_exercise::Exercise;

pub fn dispatch(state_dir: Option<String>) -> i32 {
    let exercise = match Exercise::start("waypoint", state_dir) {
        Ok(value) => value,
        Err(error) => {
            println!("error: {error}");
            return 1;
        }
    };
    let (project, cwd_text) = match exercise.register_current_project("waypoint-exercise") {
        Ok(value) => value,
        Err(error) => {
            println!("error: could not register the exercise project: {error}");
            return 1;
        }
    };
    if let Err(error) = register_loopback(&exercise) {
        println!("error: could not register private loopback provider: {error}");
        return 1;
    }
    let examples = match exercise.examples_dir() {
        Ok(value) => value,
        Err(error) => {
            println!("error: could not create examples directory: {error}");
            return 1;
        }
    };
    let client = &exercise.client;
    let anchor = submit_example(
        client,
        &examples,
        "01-anchor",
        &cwd_text,
        "anchor work for the waypoint",
    )
    .and_then(|id| create_waypoint(client, &id).map(|waypoint| (id, waypoint)));
    let (anchor_id, waypoint_id) = match anchor {
        Ok(ids) => ids,
        Err(error) => {
            println!("error: could not create the waypoint scenario: {error}");
            return 1;
        }
    };
    let candidate_id = match submit_example(
        client,
        &examples,
        "02-submitted-after-waypoint",
        &cwd_text,
        "new work that overlaps the waypoint",
    ) {
        Ok(id) => id,
        Err(error) => {
            println!("error: could not submit the overlapping scenario: {error}");
            return 1;
        }
    };
    if !has_affected_entry(client, &waypoint_id, &candidate_id) {
        println!("error: daemon did not enroll the new overlapping submission on its waypoint");
        return 1;
    }
    if let Err(error) =
        client.waypoint_patch_affected_entry(&waypoint_id, &candidate_id, "advisory")
    {
        println!("error: could not turn the enrolled example advisory: {error}");
        return 1;
    }
    if let Err(error) = client.waypoint_append_bearing(
        &waypoint_id,
        "squad",
        &anchor_id,
        "Advisory guidance for the overlapping example.",
        None,
        None,
        None,
    ) {
        println!("error: could not append advisory guidance: {error}");
        return 1;
    }
    let underway_id = match submit_underway_example(client, &examples, &project, &cwd_text) {
        Ok(id) => id,
        Err(error) => {
            println!("error: could not submit the underway scenario: {error}");
            return 1;
        }
    };
    if let Err(error) = wait_running(client, &underway_id) {
        println!("error: underway scenario did not begin running: {error}");
        return 1;
    }
    let underway_waypoint = match create_advisory_waypoint(client, &underway_id) {
        Ok(id) => id,
        Err(error) => {
            println!("error: could not create a waypoint for underway work: {error}");
            return 1;
        }
    };
    if let Err(error) = client.waypoint_append_bearing(
        &underway_waypoint,
        "squad",
        &underway_id,
        "Advisory guidance delivered after the first cell began.",
        None,
        None,
        None,
    ) {
        println!("error: could not append underway advisory guidance: {error}");
        return 1;
    }
    if let Err(error) = wait_done(client, &underway_id) {
        println!("error: underway scenario did not finish: {error}");
        return 1;
    }
    if !has_injection_delivery(client, &underway_waypoint, &underway_id) {
        println!("error: underway advisory guidance was not delivered to the next cell");
        return 1;
    }
    let completed_id = match submit_completed_example(client, &examples, &project, &cwd_text) {
        Ok(id) => id,
        Err(error) => {
            println!("error: could not submit completed redo scenario: {error}");
            return 1;
        }
    };
    if let Err(error) = wait_done(client, &completed_id) {
        println!("error: completed redo scenario did not finish: {error}");
        return 1;
    }
    let redo_waypoint = match create_advisory_waypoint(client, &completed_id) {
        Ok(id) => id,
        Err(error) => {
            println!("error: could not create completed-work waypoint: {error}");
            return 1;
        }
    };
    if let Err(error) = client.waypoint_append_bearing(
        &redo_waypoint,
        "squad",
        &completed_id,
        "Redo this completed work with the new waypoint bearing.",
        None,
        None,
        None,
    ) {
        println!("error: could not add completed-work bearing: {error}");
        return 1;
    }
    if let Err(error) = client.waypoint_redo_affected_entry(&redo_waypoint, &completed_id) {
        println!("error: could not redo completed work: {error}");
        return 1;
    }
    println!("isolated waypoint exercise is ready.");
    println!("  daemon: {}", exercise.url);
    println!("  state: {}", exercise.root.display());
    println!("  examples: {}", examples.display());
    println!("  waypoint: {waypoint_id}");
    println!("  anchor: {anchor_id}");
    println!("  enrolled blocking submission: {candidate_id}");
    println!("  advisory underway submission: {underway_id} (waypoint {underway_waypoint})");
    println!(
        "  completed then explicitly redone submission: {completed_id} (waypoint {redo_waypoint})"
    );
    println!();
    println!(
        "Inspect with: ralphus --daemon-url {} waypoint get {waypoint_id}",
        exercise.url
    );
    println!(
        "The enrollment examples are held; the underway and completed examples ran only raw local loopback commands."
    );
    println!("Activate a held enrollment scenario deliberately with:");
    println!(
        "  ralphus --daemon-url {} squad activate <squad-id>",
        exercise.url
    );
    println!(
        "This daemon uses only the state directory above; it does not read or write your normal ~/.ralphus database or settings."
    );
    0
}

fn submit_completed_example(
    client: &DaemonClient,
    directory: &std::path::Path,
    project: &str,
    cwd: &str,
) -> Result<String, String> {
    let toml = format!(
        "[[task]]\nname = \"04-completed-before-waypoint\"\nproject = {project:?}\nmachine = \"loopback:waypoint-redo\"\nno_commit_required = true\n\n[[task.cell]]\ncwd = {cwd:?}\ncommand = \"cmd /c exit 0\"\nmode = \"raw\"\n"
    );
    std::fs::write(directory.join("04-completed-before-waypoint.toml"), &toml)
        .map_err(|e| e.to_string())?;
    client
        .submit(
            &toml,
            false,
            Some("waypoint exercise: completed before waypoint"),
        )
        .map_err(|e| e.to_string())?
        .get("squad_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "submission response omitted squad_id".to_string())
}

fn submit_underway_example(
    client: &DaemonClient,
    directory: &std::path::Path,
    project: &str,
    cwd: &str,
) -> Result<String, String> {
    let toml = format!(
        "[[task]]\nname = \"03-underway-before-waypoint\"\nproject = {project:?}\nmachine = \"loopback:waypoint-underway\"\nno_commit_required = true\n\n[[task.cell]]\nid = \"wait\"\ncwd = {cwd:?}\ncommand = \"powershell -NoProfile -Command \\\"Start-Sleep -Seconds 3\\\"\"\nmode = \"raw\"\n\n[[task.cell]]\nid = \"after-guidance\"\ncwd = {cwd:?}\ncommand = \"cmd /c exit 0\"\nmode = \"raw\"\ndepends_on = [\"wait\"]\n"
    );
    std::fs::write(directory.join("03-underway-before-waypoint.toml"), &toml)
        .map_err(|e| e.to_string())?;
    client
        .submit(
            &toml,
            false,
            Some("waypoint exercise: underway before waypoint"),
        )
        .map_err(|e| e.to_string())?
        .get("squad_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "submission response omitted squad_id".to_string())
}

fn register_loopback(exercise: &Exercise) -> Result<(), String> {
    let script = loopback_script()?;
    let args = vec![script.to_string_lossy().into_owned()];
    exercise
        .client
        .register_machine(
            "loopback",
            "python",
            "Private waypoint exercise provider",
            Some(&args),
            None,
            false,
        )
        .map_err(|e| e.to_string())?;
    exercise
        .client
        .check_machine("loopback")
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn loopback_script() -> Result<PathBuf, String> {
    let path = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join("examples/providers/loopback.py");
    path.is_file().then_some(path).ok_or_else(|| {
        "examples/providers/loopback.py is unavailable from this checkout".to_string()
    })
}

fn wait_done(client: &DaemonClient, squad: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let state = client.squad(squad).map_err(|e| e.to_string())?["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if matches!(state.as_str(), "done" | "failed" | "cancelled") {
            return if state == "done" { Ok(()) } else { Err(state) };
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("timed out".to_string())
}

fn wait_running(client: &DaemonClient, squad: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let state = client.squad(squad).map_err(|e| e.to_string())?["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if state == "running" {
            return Ok(());
        }
        if matches!(state.as_str(), "done" | "failed" | "cancelled") {
            return Err(state);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("timed out".to_string())
}

fn submit_example(
    client: &DaemonClient,
    directory: &std::path::Path,
    name: &str,
    cwd: &str,
    purpose: &str,
) -> Result<String, String> {
    let toml = example_toml(name, cwd, purpose);
    std::fs::write(directory.join(format!("{name}.toml")), &toml).map_err(|e| e.to_string())?;
    client
        .submit(&toml, true, Some(&format!("waypoint exercise: {name}")))
        .map_err(|e| e.to_string())?
        .get("squad_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "submission response omitted squad_id".to_string())
}

fn create_waypoint(client: &DaemonClient, anchor_id: &str) -> Result<String, String> {
    let affected = [serde_json::json!({"kind": "squad", "entry_id": anchor_id, "mode": "block"})];
    client.waypoint_create("Coordinate overlapping work. Record advice as a bearing before activating a held example.", Some("guided waypoint exercise"), None, None, true, &affected, &[])
        .map_err(|e| e.to_string())?
        .get("id").and_then(Value::as_str).map(str::to_string)
        .ok_or_else(|| "waypoint response omitted id".to_string())
}

fn create_advisory_waypoint(client: &DaemonClient, squad_id: &str) -> Result<String, String> {
    let affected = [serde_json::json!({"kind": "squad", "entry_id": squad_id, "mode": "advisory"})];
    client
        .waypoint_create(
            "This guidance was added after the submission existed; take it into account on its next cell.",
            Some("guided underway advisory exercise"),
            None,
            None,
            true,
            &affected,
            &[],
        )
        .map_err(|e| e.to_string())?
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "waypoint response omitted id".to_string())
}

fn has_affected_entry(client: &DaemonClient, waypoint_id: &str, squad_id: &str) -> bool {
    client
        .waypoint_get(waypoint_id)
        .ok()
        .and_then(|value| value["affected"].as_array().cloned())
        .is_some_and(|entries| entries.iter().any(|entry| entry["entry_id"] == squad_id))
}

fn has_injection_delivery(client: &DaemonClient, waypoint_id: &str, squad_id: &str) -> bool {
    client
        .waypoint_deliveries(waypoint_id)
        .ok()
        .and_then(|value| {
            value
                .as_array()
                .cloned()
                .or_else(|| value["value"].as_array().cloned())
        })
        .is_some_and(|events| {
            events.iter().any(|event| {
                event["squad_id"].as_str() == Some(squad_id)
                    && event["message"]
                        .as_str()
                        .is_some_and(|message| message.starts_with("delivered "))
            })
        })
}

fn example_toml(name: &str, cwd: &str, purpose: &str) -> String {
    format!(
        "[[task]]\nname = {name:?}\n\n[[task.cell]]\ncwd = {cwd:?}\ncommand = \"echo ralphus waypoint exercise\"\nremediation_attempts = 1\n\n# {purpose}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn examples_are_held_command_cells_with_the_requested_working_directory() {
        let text = example_toml("scenario", "C:/project", "purpose");
        assert!(text.contains("command = \"echo ralphus waypoint exercise\""));
        assert!(text.contains("remediation_attempts = 1"));
        assert!(text.contains("cwd = \"C:/project\""));
    }
}
