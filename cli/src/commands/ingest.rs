//! `ralphus ingest <branch>... [--review <selector>] [--pr <n>] [--base <branch>] [--yes]`
//! (RAL-702): attach existing branches, and any open PRs on them, to a review.

use std::io::IsTerminal as _;

use serde_json::{Value, json};

use crate::args::GlobalOpts;
use crate::client::DaemonClient;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;
use crate::selector::{DEFAULT_REVIEW_LIST_HINT, resolve_guardian_selector};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IngestArgs {
    pub branches: Vec<String>,
    pub review: Option<String>,
    pub pr: Option<u64>,
    pub base: Option<String>,
    pub prompt: Option<String>,
    pub prompt_file: Option<String>,
    pub label: Option<String>,
    pub yes: bool,
    pub ralphus_extras: bool,
    pub infer_live_actions: bool,
    pub extras_exhibits: bool,
    pub extras_insights: bool,
}

pub fn parse_ingest(scanner: &mut Scanner) -> super::Command {
    let yes = scanner.take_bool("--yes");
    let ralphus_extras = scanner.take_bool("--ralphus-extras");
    let infer_live_actions = scanner.take_bool("--infer-live-actions");
    let extras_exhibits = scanner.take_bool("--extras-exhibits");
    let extras_insights = scanner.take_bool("--extras-insights");
    let review = scanner.take_value("--review").ok().flatten();
    let pr = scanner
        .take_value("--pr")
        .ok()
        .flatten()
        .and_then(|v| v.trim_start_matches('#').parse().ok());
    let base = scanner.take_value("--base").ok().flatten();
    let prompt = scanner.take_value("--prompt").ok().flatten();
    let prompt_file = scanner.take_value("--prompt-file").ok().flatten();
    let label = scanner.take_value("--label").ok().flatten();
    super::Command::Ingest(IngestArgs {
        branches: scanner.clone().remaining(),
        review,
        pr,
        base,
        prompt,
        prompt_file,
        label,
        yes,
        ralphus_extras,
        infer_live_actions,
        extras_exhibits,
        extras_insights,
    })
}

/// The root of the git checkout the command runs in.
fn git_toplevel() -> Result<String, CommandError> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| CommandError::Usage(format!("could not run git: {e}")))?;
    if !out.status.success() {
        return Err(CommandError::Usage(
            "ingest must run inside a git repository".to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Builds the request body shared by the dry run and the creating call.
pub fn build_request(
    client: &DaemonClient,
    args: &IngestArgs,
    path: &str,
) -> Result<Value, CommandError> {
    if args.branches.is_empty() {
        return Err(CommandError::Usage(
            "ingest needs at least one branch".to_string(),
        ));
    }
    let prompt = match (&args.prompt, &args.prompt_file) {
        (Some(_), Some(_)) => {
            return Err(CommandError::Usage(
                "--prompt and --prompt-file are mutually exclusive".to_string(),
            ));
        }
        (Some(p), None) => Some(p.clone()),
        (None, Some(f)) => Some(
            std::fs::read_to_string(f)
                .map_err(|e| CommandError::Usage(format!("cannot read {f}: {e}")))?,
        ),
        (None, None) => None,
    };
    let review_id = match &args.review {
        Some(sel) => {
            Some(resolve_guardian_selector(client, sel, DEFAULT_REVIEW_LIST_HINT)?.guardian_id)
        }
        None => None,
    };
    let mut body = json!({"path": path, "branches": args.branches});
    let map = body.as_object_mut().expect("object");
    if let Some(v) = review_id {
        map.insert("review_id".into(), json!(v));
    }
    if let Some(v) = args.pr {
        map.insert("pr".into(), json!(v));
    }
    if let Some(v) = &args.base {
        map.insert("base".into(), json!(v));
    }
    if let Some(v) = prompt {
        map.insert("prompt".into(), json!(v));
    }
    if let Some(v) = &args.label {
        map.insert("label".into(), json!(v));
    }
    for (flag, key) in [
        (args.ralphus_extras, "ralphus_extras"),
        (args.infer_live_actions, "infer_live_actions"),
        (args.extras_exhibits, "extras_exhibits"),
        (args.extras_insights, "extras_insights"),
    ] {
        if flag {
            map.insert(key.into(), json!(true));
        }
    }
    Ok(body)
}

fn print_plan(plan: &Value) {
    println!(
        "base:  {} ({})",
        plan["base"].as_str().unwrap_or("?"),
        plan["base_rule"].as_str().unwrap_or("?")
    );
    let model = plan["model"]
        .as_str()
        .map(|m| format!(" / {m}"))
        .unwrap_or_default();
    println!(
        "agent: {}{} ({})",
        plan["agent"].as_str().unwrap_or("?"),
        model,
        plan["agent_rule"].as_str().unwrap_or("?")
    );
    for b in plan["branches"].as_array().into_iter().flatten() {
        let name = b["branch"].as_str().unwrap_or("?");
        match b["pr_number"].as_u64() {
            Some(n) => println!("  {name}: will adopt PR #{n}"),
            None => println!("  {name}: no open PR"),
        }
    }
    let extras = &plan["extras"];
    for (key, label) in [
        ("live_actions", "inferred live actions"),
        ("exhibits", "exhibits"),
        ("insights", "model-filtered insights"),
    ] {
        if extras[key].as_bool() == Some(true) {
            println!("extra: {label}");
        }
    }
    for w in plan["warnings"].as_array().into_iter().flatten() {
        println!("warning: {}", w.as_str().unwrap_or_default());
    }
    if let Some(w) = plan["force_push_warning"].as_str() {
        println!("warning: {w}");
    }
}

fn confirm(prompt: &str) -> bool {
    use std::io::Write as _;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

pub fn cmd_ingest(opts: &GlobalOpts, args: IngestArgs) -> i32 {
    run_and_report(opts, None, || {
        let client = opts.client();
        let path = git_toplevel()?;
        let mut body = build_request(&client, &args, &path)?;
        body["dry_run"] = json!(true);
        let plan = client.ingest(&body)?;
        if !opts.json {
            print_plan(&plan);
        }
        if !args.yes {
            if !std::io::stdin().is_terminal() {
                return Err(CommandError::Usage(
                    "ingest needs confirmation; pass --yes when not running interactively"
                        .to_string(),
                ));
            }
            if !confirm("Proceed? [y/N] ") {
                println!("aborted");
                return Ok(());
            }
        }
        body["dry_run"] = json!(false);
        let result = client.ingest(&body)?;
        emit(opts, &result, |r| {
            println!(
                "ingested into review {} (squad {})",
                r["review_id"].as_str().unwrap_or("?"),
                r["squad_id"].as_str().unwrap_or("?")
            );
        });
        Ok(())
    })
}
