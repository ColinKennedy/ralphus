//! `ralphus initialize mailbox`: deterministic failure and remediation exercise.
//!
//! Two tasks fail on purpose -- one in its cell, one in its proof -- and the
//! exercise verifies both failures reach the mailbox carrying remediation
//! guidance. With `--remote`, both run on the strict loopback machine.

use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::commands::initialize::exercise::{Exercise, ExerciseOptions, Fixture, squad_id};

pub fn dispatch(options: &ExerciseOptions) -> i32 {
    let exercise = match Exercise::start("mailbox", options) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let fixture = match exercise.fixture_project("mailbox") {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let examples = match exercise.examples_dir() {
        Ok(value) => value,
        Err(error) => return fail(&format!("could not create examples: {error}")),
    };
    let client_id = match exercise.client.mailbox_register() {
        Ok(value) => match value["client_id"].as_str() {
            Some(id) => id.to_string(),
            None => return fail("mailbox register omitted client_id"),
        },
        Err(error) => return fail(&format!("could not register mailbox client: {error}")),
    };
    let mut squads = Vec::new();
    for (name, toml) in [
        ("cell-failure", cell_failure_toml(&exercise, &fixture)),
        ("proof-failure", proof_failure_toml(&exercise, &fixture)),
    ] {
        let squad = match submit(&exercise.client, &examples, name, &toml) {
            Ok(value) => value,
            Err(error) => return fail(&error),
        };
        match exercise.wait_terminal(&squad, Duration::from_secs(120)) {
            Ok(state) if state == "failed" => squads.push(squad),
            Ok(state) => {
                return fail(&format!(
                    "{name} squad {squad} was meant to fail but ended {state}"
                ));
            }
            Err(error) => return fail(&error),
        }
    }
    let messages = match wait_for_messages(&exercise.client, &client_id) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let ids: Vec<String> = messages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect();
    if ids.len() < 2
        || !messages.as_array().into_iter().flatten().all(|m| {
            m["message"].as_str().is_some_and(|message| {
                message.contains("Suggested next step:")
                    || message.contains("Manual intervention required:")
                    || message.contains("Automatic fix applied:")
            })
        })
    {
        return fail("expected failure messages with remediation guidance");
    }
    if let Err(error) = exercise.client.mailbox_drain(&client_id, Some(&ids)) {
        return fail(&format!("could not drain verified messages: {error}"));
    }
    println!("isolated mailbox exercise is ready.");
    println!("  daemon: {}", exercise.url);
    println!("  state: {}", exercise.root.display());
    println!("  failed cell squad: {}", squads[0]);
    println!("  failed proof squad: {}", squads[1]);
    println!(
        "  verified and drained {} remediation-bearing messages",
        ids.len()
    );
    println!(
        "Inspect all messages with: ralphus --daemon-url {} mailbox check",
        exercise.url
    );
    0
}

fn fail(message: &str) -> i32 {
    println!("error: {message}");
    1
}

fn submit(
    client: &crate::client::DaemonClient,
    dir: &std::path::Path,
    name: &str,
    toml: &str,
) -> Result<String, String> {
    std::fs::write(dir.join(format!("{name}.toml")), toml).map_err(|e| e.to_string())?;
    client
        .submit(toml, false, Some(&format!("mailbox exercise: {name}")))
        .map_err(|e| e.to_string())
        .and_then(|v| squad_id(&v))
}

fn wait_for_messages(client: &crate::client::DaemonClient, id: &str) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let messages = client
            .mailbox_messages(id, true, None)
            .map_err(|e| e.to_string())?;
        if messages.as_array().is_some_and(|items| items.len() >= 2) {
            return Ok(messages);
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("failure messages did not reach the mailbox".to_string())
}

// `exit N` means the same thing in `cmd` and `sh`, the two shells the exercise
// daemon pins `RALPHUS_SHELL` to.
fn cell_failure_toml(exercise: &Exercise, fixture: &Fixture) -> String {
    let project = &fixture.name;
    let machine = exercise.machine_line();
    let cwd = exercise.cell_cwd(fixture, "mailbox-cell-failure");
    format!(
        "[[task]]\nname = \"mailbox-cell-failure\"\nproject = {project:?}\n{machine}no_commit_required = true\n\n[[task.cell]]\ncwd = {cwd:?}\ncommand = \"exit 1\"\nmode = \"raw\"\n"
    )
}

fn proof_failure_toml(exercise: &Exercise, fixture: &Fixture) -> String {
    let project = &fixture.name;
    let machine = exercise.machine_line();
    let cwd = exercise.cell_cwd(fixture, "mailbox-proof-failure");
    format!(
        "[[task]]\nname = \"mailbox-proof-failure\"\nproject = {project:?}\n{machine}no_commit_required = true\n\n[[task.cell]]\ncwd = {cwd:?}\ncommand = \"exit 0\"\nmode = \"raw\"\n\n[[task.cell.proof]]\ncommand = \"exit 1\"\nmode = \"raw\"\n"
    )
}
