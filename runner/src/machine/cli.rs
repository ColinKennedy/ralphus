//! `ralphus-runner machine ...`:
//!
//! ```text
//! ralphus-runner machine <verb> --remote-root <path>   request on stdin, reply on stdout
//! ralphus-runner machine supervise --job <dir>         run one job (started by `exec`)
//! ```
//!
//! stdout carries exactly one JSON reply line; events go to stderr as
//! `RALPHUS_EVENT: ` lines, the shape every provider uses.

use std::io::Read as _;
use std::process::ExitCode;

use serde_json::{Value, json};

use super::{Context, EVENT_PREFIX, PROTOCOL_VERSION, execute};

fn value_of(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Run `machine` with `args` (everything after `machine`).
#[must_use]
pub fn run(args: &[String]) -> ExitCode {
    let Some(verb) = args.first().filter(|v| !v.starts_with('-')) else {
        reply(&json!({"ok": false, "protocol_version": PROTOCOL_VERSION,
            "error": "usage: ralphus-runner machine <verb> --remote-root <path>"}));
        return ExitCode::from(2);
    };
    if verb == "supervise" {
        let Some(job) = value_of(args, "--job") else {
            eprintln!("ralphus-runner machine supervise needs --job <dir>");
            return ExitCode::from(2);
        };
        super::jobs::supervise(std::path::Path::new(&job));
        return ExitCode::SUCCESS;
    }
    let Some(remote_root) = value_of(args, "--remote-root") else {
        reply(&json!({"ok": false, "protocol_version": PROTOCOL_VERSION,
            "error": format!("machine {verb} needs --remote-root <path>")}));
        return ExitCode::from(2);
    };
    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        reply(&json!({"ok": false, "protocol_version": PROTOCOL_VERSION,
            "error": format!("could not read the request on stdin: {e}")}));
        return ExitCode::FAILURE;
    }
    let request: Value = if text.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(e) => {
                reply(&json!({"ok": false, "protocol_version": PROTOCOL_VERSION,
                    "error": format!("could not parse the {verb} request on stdin: {e}")}));
                return ExitCode::FAILURE;
            }
        }
    };
    let context = match Context::for_current_exe(&remote_root, &[]) {
        Ok(context) => context,
        Err(e) => {
            reply(&json!({"ok": false, "protocol_version": PROTOCOL_VERSION, "error": e}));
            return ExitCode::FAILURE;
        }
    };
    let outcome = execute(verb, &request, &context);
    for event in &outcome.events {
        eprintln!("{EVENT_PREFIX} {event}");
    }
    reply(&outcome.body);
    ExitCode::SUCCESS
}

/// The one reply line on stdout.
#[allow(clippy::print_stdout)]
fn reply(body: &Value) {
    println!("{body}");
}
