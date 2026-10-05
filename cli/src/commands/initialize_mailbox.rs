//! `ralphus initialize mailbox`: deterministic failure and remediation exercise.

use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::commands::initialize_exercise::Exercise;

pub fn dispatch(state_dir: Option<String>) -> i32 {
    let exercise = match Exercise::start("mailbox", state_dir) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let (_, cwd) = match exercise.register_current_project("mailbox-exercise") {
        Ok(value) => value,
        Err(error) => return fail(&format!("could not register exercise project: {error}")),
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
    let cell = submit(
        &exercise.client,
        &examples,
        "cell-failure",
        &cell_failure_toml(&cwd),
    );
    let cell = match cell {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    if let Err(error) = wait_terminal(&exercise.client, &cell) {
        return fail(&error);
    }
    let proof = submit(
        &exercise.client,
        &examples,
        "proof-failure",
        &proof_failure_toml(&cwd),
    );
    let proof = match proof {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    if let Err(error) = wait_terminal(&exercise.client, &proof) {
        return fail(&error);
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
    println!("  failed cell squad: {cell}");
    println!("  failed proof squad: {proof}");
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
        .map_err(|e| e.to_string())?
        .get("squad_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "submission omitted squad_id".to_string())
}

fn wait_terminal(client: &crate::client::DaemonClient, squad: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let value = client.squad(squad).map_err(|e| e.to_string())?;
        if matches!(
            value["state"].as_str(),
            Some("done" | "failed" | "cancelled")
        ) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(format!("{squad} did not reach a terminal state"))
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

fn cell_failure_toml(cwd: &str) -> String {
    format!(
        "[[task]]\nname = \"mailbox-cell-failure\"\n\n[[task.cell]]\ncwd = {cwd:?}\ncommand = \"cmd /c exit 1\"\nmode = \"raw\"\n"
    )
}
fn proof_failure_toml(cwd: &str) -> String {
    format!(
        "[[task]]\nname = \"mailbox-proof-failure\"\n\n[[task.cell]]\ncwd = {cwd:?}\ncommand = \"cmd /c exit 0\"\nmode = \"raw\"\n\n[[task.cell.proof]]\ncommand = \"cmd /c exit 1\"\nmode = \"raw\"\n"
    )
}
