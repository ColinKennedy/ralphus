//! `ralphus initialize followup`: the deferred-prophecy follow-up flow, end to
//! end and without a model.
//!
//! A real squad runs on an isolated daemon, its review merges, the merged
//! review offers follow-up work for the `deferred` prophecy its cell wrote,
//! the offer is accepted, and the follow-up squad the daemon drafts runs to
//! completion. Everything but two things is
//! the real system: the daemon, its SQLite store, the scheduler, the runner,
//! git worktrees, the prophecy store, and squad submission.
//!
//! The two stand-ins:
//!
//! - **The agent.** The cells name a `raw` agent profile whose executable is a
//!   copy of this binary under the name [`STUB_AGENT_FILE`]. Invoked that way,
//!   [`run_exercise_agent`] makes one commit, pushes it, and prints the
//!   `RALPHUS_PROPHECY:` lines a real agent would. The runner, the scheduler's
//!   prophecy recording and the git work around it are not simulated.
//! - **The forge merge.** A review reaches `merged` when its pull request
//!   merges on a forge. The exercise has no forge, so it does what a person
//!   merging outside ralphus does -- fast-forwards the base branch to the
//!   review branch with git -- and the daemon notices through its own
//!   already-in-base check (`review merge`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::client::DaemonClient;
use crate::commands::initialize::exercise::{Exercise, ExerciseOptions, Fixture, squad_id};

/// The file name this binary is copied to so it can act as the agent.
const STUB_AGENT_FILE: &str = "ralphus-exercise-agent";

/// The agent profile the exercise's cells name.
const STUB_AGENT_PROFILE: &str = "followup-stub";

/// How long any one step waits before the exercise gives up.
const STEP_TIMEOUT: Duration = Duration::from_secs(180);

/// The raw command proof on the original cell. The follow-up squad must carry it.
const ORIGINAL_PROOF: &str = "git rev-parse HEAD";

/// The remediating command proof on the original cell: it keeps its default
/// mode and carries a retry budget, which the follow-up must keep too.
const REMEDIATING_PROOF: &str = "git rev-parse --verify HEAD";

/// How many remediation attempts the remediating proof is given.
const REMEDIATION_ATTEMPTS: i64 = 2;

/// The scored prompt proof on the original cell. The stub agent answers it with
/// an appraisal, and the follow-up must carry the prompt and its threshold.
const PROMPT_PROOF: &str = "Appraise whether the work on this branch is complete.";

/// The score the prompt proof needs to pass.
const PASS_SCORE: i64 = 5;

/// What the stub agent says it deferred in the original work.
const DEFERRED_NOTE: &str = "document feature.txt in the README";

/// What the stub agent says it deferred in the follow-up work. The daemon's
/// depth cap must keep this from being offered.
const NESTED_DEFERRED_NOTE: &str = "nested follow-up that must not be offered again";

/// What the stub agent could not check. It is `unconfirmed`, so it must never
/// reach the offer.
const UNCONFIRMED_NOTE: &str = "the full test suite was not run";

pub fn dispatch(options: &ExerciseOptions) -> i32 {
    match run(options) {
        Ok(()) => {
            println!("followup exercise passed");
            0
        }
        Err(error) => {
            println!("error: {error}");
            1
        }
    }
}

fn step(message: &str) {
    println!("ok: {message}");
}

fn run(options: &ExerciseOptions) -> Result<(), String> {
    if options.remote {
        return Err(
            "initialize followup runs on the local machine only: the review merge it drives is a \
             git fast-forward in the daemon's own checkout"
                .to_string(),
        );
    }
    let exercise = Exercise::start("followup", options)?;
    let fixture = exercise.fixture_project("followup")?;
    let client = &exercise.client;
    let agent = install_stub_agent(&exercise.root)?;
    client
        .create_agent_profile(
            STUB_AGENT_PROFILE,
            "raw",
            Some(&agent.to_string_lossy()),
            None,
            &[],
            None,
        )
        .map_err(|e| format!("could not register the stub agent profile: {e}"))?;

    // Follow-up cells are pooled by the Arbiter under whichever triage type it
    // gives them (the built-in `feature`, or `unclassified` when it cannot
    // classify). A threshold of one drains either pool as soon as a follow-up
    // cell lands. A drained pool needs its type's upstream for this project,
    // so both types target the fixture's `main` branch, as the review does.
    for triage_type in ["feature", "unclassified"] {
        client
            .set_triage_type_upstream(&fixture.name, triage_type, Some("main"))
            .map_err(|e| format!("could not set the {triage_type} upstream: {e}"))?;
        client
            .set_triage_pool_threshold(&fixture.name, triage_type, Some(1))
            .map_err(|e| format!("could not set the {triage_type} pool threshold: {e}"))?;
    }

    // 1. Ordinary work: one squad, one review, one deferred prophecy.
    let original = client
        .submit(
            &original_toml(&fixture.name),
            false,
            Some("followup exercise: original work"),
        )
        .map_err(|e| e.to_string())
        .and_then(|v| squad_id(&v))?;
    expect_done(&exercise, &original)?;
    let review = wait_for_review(client, &[])?;
    wait_for_status(client, &review, "in_review")?;
    step("original squad finished and its review is in_review");

    let kinds = prophecy_kinds(client, &original)?;
    for kind in ["deferred", "unconfirmed", "decision"] {
        if !kinds.iter().any(|k| k == kind) {
            return Err(format!(
                "the stub agent's `{kind}` prophecy was not recorded (saw {kinds:?})"
            ));
        }
    }
    step("the agent's prophecies were recorded");

    // No offer before the review merges.
    expect_no_offer(client, &review)?;

    // 2. The review merges: the base branch absorbs the review branch.
    merge_review_into_main(client, &fixture, &review)?;
    step("the review merged");

    // 3. The merge offered follow-up work for the deferred prophecy only.
    let offer = client
        .guardian_followup(&review)
        .map_err(|e| format!("the merged review sent no follow-up offer: {e}"))?;
    expect(offer["status"] == "offered", "the offer is open", &offer)?;
    let items = offer["items"].as_array().cloned().unwrap_or_default();
    expect(
        items.len() == 1 && items[0]["body"] == DEFERRED_NOTE,
        "only the deferred prophecy is offered, not the unconfirmed or decision ones",
        &offer,
    )?;
    expect(
        items[0]["agent"] == STUB_AGENT_PROFILE,
        "the offer remembers the agent that deferred the work",
        &offer,
    )?;
    step("the merge offered a follow-up for the deferred prophecy only");

    // 4. Accepting drafts a squad that starts at once (auto-start is on by
    //    default); it creates no waypoint.
    let accepted = client
        .guardian_followup_accept(&review)
        .map_err(|e| format!("accepting the offer failed: {e}"))?;
    let followup = accepted["squad_id"]
        .as_str()
        .ok_or("accept named no squad")?
        .to_string();
    expect(
        accepted.get("waypoint_id").is_none(),
        "accepting creates no waypoint",
        &accepted,
    )?;
    expect(
        accepted["base"]
            .as_str()
            .is_some_and(|base| base.ends_with("main")),
        "the follow-up is based on the branch the review merged into",
        &accepted,
    )?;
    expect(
        accepted["started"] == true,
        "the follow-up starts at once by default",
        &accepted,
    )?;
    step("accepting created a follow-up squad and no waypoint");

    // The follow-up has no review of its own to submit to, and its cell keeps
    // the proof of the work it follows up on.
    let squad_view = client.squad(&followup).map_err(|e| e.to_string())?;
    expect(
        squad_view["reviews"]
            .as_array()
            .is_none_or(std::vec::Vec::is_empty),
        "the follow-up squad does not create a review of its own",
        &squad_view["reviews"],
    )?;
    expect(
        squad_view.to_string().contains(ORIGINAL_PROOF),
        "the follow-up cell inherited the original cell's raw command proof",
        &squad_view,
    )?;
    // The API does not report a proof's mode, attempt budget or pass score, so
    // they are checked by what they cause. A remediating command with no
    // `remediation_attempts` would have been rejected when the squad was
    // submitted; the scored prompt proof is taught the appraisal marker, where
    // an unscored one is taught the plain verdict marker.
    expect(
        find_proof(&squad_view, "spec", REMEDIATING_PROOF)
            .is_some_and(|proof| proof["kind"] == "command"),
        "the follow-up kept the remediating command proof",
        &squad_view["tasks"][0]["cells"][0]["proof"],
    )?;
    let scored = find_proof(&squad_view, "spec", PROMPT_PROOF);
    expect(
        scored.is_some_and(|proof| {
            proof["kind"] == "prompt"
                && proof["system_prompt"]
                    .as_str()
                    .is_some_and(|text| text.contains("RALPHUS_APPRAISAL"))
        }),
        "the follow-up kept the prompt proof and its pass score",
        &scored,
    )?;
    step(
        "the follow-up squad has no review of its own and kept the raw, remediating and \
         scored prompt proofs",
    );

    let again = client.guardian_followup_accept(&review);
    expect(
        again.as_ref().err().and_then(|e| e.status_code) == Some(409),
        "a second accept is refused",
        &format!("{again:?}"),
    )?;

    // 5. The follow-up runs to completion.
    expect_done(&exercise, &followup)?;
    step("the follow-up squad ran to completion");

    // Each inherited proof ran, and the scored one passed on the stub's
    // appraisal alone: without its threshold it would have had no verdict.
    let finished = client.squad(&followup).map_err(|e| e.to_string())?;
    let states: Vec<&str> = finished["tasks"][0]["cells"][0]["proof"]
        .as_array()
        .map(|proofs| proofs.iter().filter_map(|p| p["state"].as_str()).collect())
        .unwrap_or_default();
    expect(
        states.len() == 3 && states.iter().all(|state| *state == "done"),
        "all three inherited proofs ran and passed in the follow-up",
        &states,
    )?;
    step("the follow-up's raw, remediating and scored proofs all passed");

    // 6. The follow-up's cell drained the Arbiter's pool into an automatic
    //    review: the daemon, not the follow-up's own task file, built it.
    let pooled_review = wait_for_review(client, &[review.as_str()])?;
    let pooled = client
        .guardian_get(&pooled_review)
        .map_err(|e| e.to_string())?;
    expect(
        pooled["origin"] == "arbiter",
        "the follow-up's review was built by the Arbiter from its pool",
        &pooled["origin"],
    )?;
    step("the Arbiter built an automatic review from the follow-up's pooled cell");

    // 7. The follow-up wrote a deferred prophecy of its own; the depth cap
    //    keeps it from chaining into another offer.
    let nested = prophecy_bodies(client, &followup)?;
    expect(
        nested.iter().any(|b| b == NESTED_DEFERRED_NOTE),
        "the follow-up's own deferred prophecy was recorded",
        &format!("{nested:?}"),
    )?;
    expect_no_offer(client, &pooled_review)?;
    step("a follow-up's own deferrals are not offered again");
    Ok(())
}

/// The first object anywhere under `value` whose `key` field is exactly `text`.
fn find_proof<'a>(value: &'a Value, key: &str, text: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if map.get(key).and_then(Value::as_str) == Some(text) {
                return Some(value);
            }
            map.values().find_map(|child| find_proof(child, key, text))
        }
        Value::Array(items) => items.iter().find_map(|child| find_proof(child, key, text)),
        _ => None,
    }
}

fn expect(condition: bool, what: &str, context: &dyn std::fmt::Debug) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(format!("expected: {what}\n  saw: {context:?}"))
    }
}

fn original_toml(project: &str) -> String {
    format!(
        "[[task]]\nname = \"feature\"\nproject = {project:?}\n\n[[task.cell]]\n\
         cwd = \"<<ralphus:new-worktree/followup-feature?upstream=<<default>>>>\"\n\
         prompt = \"Add feature.txt. (The exercise's stub agent ignores this text.)\"\n\
         agent = {STUB_AGENT_PROFILE:?}\nreview = \"<<ralphus:new-review/original>>\"\n\n\
         [[task.cell.proof]]\ncommand = {ORIGINAL_PROOF:?}\nmode = \"raw\"\n\n\
         [[task.cell.proof]]\ncommand = {REMEDIATING_PROOF:?}\n\
         remediation_attempts = {REMEDIATION_ATTEMPTS}\nagent = {STUB_AGENT_PROFILE:?}\n\n\
         [[task.cell.proof]]\nprompt = {PROMPT_PROOF:?}\npass_score = {PASS_SCORE}\n\
         agent = {STUB_AGENT_PROFILE:?}\n\n\
         [[review]]\nid = \"ralphus:new-review/original\"\nupstream = \"main\"\n\
         proof_scope = \"nothing\"\nskip_manual_checks = true\n"
    )
}

/// Copies this binary to `<root>/ralphus-exercise-agent[.exe]`, the file the
/// stub agent profile points at.
fn install_stub_agent(root: &Path) -> Result<PathBuf, String> {
    let source = std::env::current_exe().map_err(|e| format!("could not find this binary: {e}"))?;
    let target = root.join(format!("{STUB_AGENT_FILE}{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&source, &target).map_err(|e| {
        format!(
            "could not install the stub agent at {}: {e}",
            target.display()
        )
    })?;
    Ok(target)
}

/// Whether this process was started as the exercise's stub agent.
#[must_use]
pub fn invoked_as_exercise_agent() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy() == STUB_AGENT_FILE)
        })
        .unwrap_or(false)
}

/// The stub agent: one commit pushed on the cell's own branch, then the
/// prophecy lines a real agent would write. Which work it is doing is read
/// from the prompt it was handed -- the daemon's follow-up draft says
/// "follow-up work", the exercise's original prompt does not.
#[must_use]
pub fn run_exercise_agent(args: &[String]) -> i32 {
    let prompt = args.last().map(String::as_str).unwrap_or_default();
    // A scored prompt proof asks for an appraisal rather than for work.
    if prompt.contains(PROMPT_PROOF) || prompt.contains("RALPHUS_APPRAISAL") {
        println!(
            "RALPHUS_APPRAISAL: {{\"score\": {}, \"summary\": \"the stub agent approves\", \
             \"sections\": []}}",
            PASS_SCORE + 3
        );
        return 0;
    }
    let followup = prompt.contains("follow-up work");
    let file = if followup {
        "followup.txt"
    } else {
        "feature.txt"
    };
    let git = |args: &[&str]| {
        Command::new("git")
            .args([
                "-c",
                "user.email=exercise@example.invalid",
                "-c",
                "user.name=Exercise",
            ])
            .args(args)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    if std::fs::write(file, format!("{file}\n")).is_err()
        || !git(&["add", file])
        || !git(&["commit", "-q", "-m", file])
        || !git(&["push", "-q", "-u", "origin", "HEAD"])
    {
        println!("the stub agent could not commit and push {file}");
        return 1;
    }
    println!("The stub agent added {file}.");
    if followup {
        println!("RALPHUS_PROPHECY: deferred: {NESTED_DEFERRED_NOTE}");
    } else {
        println!("RALPHUS_PROPHECY: deferred: {DEFERRED_NOTE}");
        println!("RALPHUS_PROPHECY: unconfirmed: {UNCONFIRMED_NOTE}");
        println!("RALPHUS_PROPHECY: decision: kept the feature to a single file");
    }
    0
}

fn poll<T>(what: &str, mut check: impl FnMut() -> Result<Option<T>, String>) -> Result<T, String> {
    let deadline = Instant::now() + STEP_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(value) = check()? {
            return Ok(value);
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err(format!("timed out waiting for {what}"))
}

fn expect_done(exercise: &Exercise, squad: &str) -> Result<(), String> {
    let state = exercise.wait_terminal(squad, STEP_TIMEOUT)?;
    if state == "done" {
        return Ok(());
    }
    Err(format!("squad {squad} ended {state}, not done"))
}

/// The first review that is not in `known`.
fn wait_for_review(client: &DaemonClient, known: &[&str]) -> Result<String, String> {
    poll("a review to form", || {
        let list = client.guardian_list().map_err(|e| e.to_string())?;
        Ok(list.as_array().and_then(|reviews| {
            reviews
                .iter()
                .filter_map(|r| r["id"].as_str())
                .find(|id| !known.contains(id))
                .map(str::to_string)
        }))
    })
}

fn wait_for_status(client: &DaemonClient, review: &str, want: &str) -> Result<(), String> {
    let mut last = String::new();
    poll(&format!("review {review} to be {want}"), || {
        let view = client.guardian_get(review).map_err(|e| e.to_string())?;
        last = view["status"].as_str().unwrap_or_default().to_string();
        if matches!(last.as_str(), "merge_failed" | "cancelled") {
            return Err(format!(
                "review {review} ended {last}: {}",
                view["detail"].as_str().unwrap_or("no detail")
            ));
        }
        Ok((last == want).then_some(()))
    })
    .map_err(|e| format!("{e} (last status {last})"))
}

fn prophecy_rows(client: &DaemonClient, squad: &str) -> Result<Vec<Value>, String> {
    let rows = client
        .list_prophecies(crate::client::ProphecyFilters {
            entity_uri: None,
            squad_id: Some(squad),
            guardian_id: None,
            limit: 100,
            offset: 0,
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.as_array().cloned().unwrap_or_default())
}

fn prophecy_kinds(client: &DaemonClient, squad: &str) -> Result<Vec<String>, String> {
    Ok(prophecy_rows(client, squad)?
        .iter()
        .filter_map(|row| row["kind"].as_str().map(str::to_string))
        .collect())
}

fn prophecy_bodies(client: &DaemonClient, squad: &str) -> Result<Vec<String>, String> {
    Ok(prophecy_rows(client, squad)?
        .iter()
        .filter_map(|row| row["body"].as_str().map(str::to_string))
        .collect())
}

fn expect_no_offer(client: &DaemonClient, review: &str) -> Result<(), String> {
    match client.guardian_followup(review) {
        Err(error) if error.status_code == Some(404) => Ok(()),
        other => Err(format!(
            "review {review} should have no follow-up offer, got {other:?}"
        )),
    }
}

/// Fast-forwards the fixture's base branch to `review`'s branch -- the merge a
/// person makes on a forge -- then asks the daemon to notice. Its own
/// already-in-base check marks the review merged.
fn merge_review_into_main(
    client: &DaemonClient,
    fixture: &Fixture,
    review: &str,
) -> Result<(), String> {
    let view = client.guardian_get(review).map_err(|e| e.to_string())?;
    let branch = view["review_branch"]
        .as_str()
        .filter(|b| !b.is_empty())
        .ok_or_else(|| format!("review {review} has no review branch yet: {view}"))?
        .to_string();
    git(&fixture.path, &["merge", "--ff-only", &branch])?;
    git(&fixture.path, &["push", "-q", "origin", "main"])?;
    client
        .guardian_merge(review)
        .map_err(|e| format!("review merge failed: {e}"))?;
    wait_for_status(client, review, "merged")
}

fn git(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args([
            "-c",
            "user.email=exercise@example.invalid",
            "-c",
            "user.name=Exercise",
        ])
        .args(args)
        .output()
        .map_err(|e| format!("could not run git {args:?}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {args:?} failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_original_squad_is_a_valid_task_file_on_the_stub_agent() {
        let toml = original_toml("proj");
        let report = ralphus_core::validate::validate_toml(&toml);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let file: ralphus_core::schema::TaskFile = toml::from_str(&toml).unwrap();
        assert_eq!(file.task.len(), 1);
        assert_eq!(file.review.len(), 1);
        assert_eq!(
            file.task[0].cell[0].agent,
            Some(ralphus_core::schema::AgentSpec::Single(
                STUB_AGENT_PROFILE.to_string()
            ))
        );
    }

    #[test]
    fn the_stub_agent_is_not_mistaken_for_the_cli() {
        // The test binary is not named like the stub, so a normal run never
        // takes the agent path.
        assert!(!invoked_as_exercise_agent());
    }
}
