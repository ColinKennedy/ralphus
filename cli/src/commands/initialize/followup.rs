//! `ralphus initialize followup`: the deferred-prophecy follow-up flow, end to
//! end and without a model.
//!
//! A real squad runs on an isolated daemon, its review merges, the merged
//! review offers follow-up work for the `deferred` prophecy its cell wrote,
//! the offer is accepted, and the follow-up squad the daemon drafts runs to
//! completion under the waypoint that explains it. Everything but two things is
//! the real system: the daemon, its SQLite store, the scheduler, the runner,
//! git worktrees, the prophecy store, the waypoint store, and squad submission.
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
    //    default) and a blocking waypoint.
    let accepted = client
        .guardian_followup_accept(&review)
        .map_err(|e| format!("accepting the offer failed: {e}"))?;
    let followup = accepted["squad_id"]
        .as_str()
        .ok_or("accept named no squad")?
        .to_string();
    let waypoint = accepted["waypoint_id"]
        .as_str()
        .ok_or("accept named no waypoint")?
        .to_string();
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
    step("accepting created a follow-up squad and a waypoint");

    let again = client.guardian_followup_accept(&review);
    expect(
        again.as_ref().err().and_then(|e| e.status_code) == Some(409),
        "a second accept is refused",
        &format!("{again:?}"),
    )?;

    let detail = client.waypoint_get(&waypoint).map_err(|e| e.to_string())?;
    expect(detail["state"] == "open", "the waypoint is open", &detail)?;
    let roster = detail["roster"].as_array().cloned().unwrap_or_default();
    expect(
        roster.len() == 1 && roster[0]["entry_id"] == review.as_str(),
        "the merged review is the waypoint's roster",
        &detail,
    )?;
    let affected = detail["affected"].as_array().cloned().unwrap_or_default();
    expect(
        affected.len() == 1
            && affected[0]["mode"] == "block"
            && affected[0]["bearing_decision"] == "accepted",
        "the follow-up squad is the waypoint's blocking, already-answered affected entry",
        &detail,
    )?;
    step("the waypoint lists the review as roster and the squad as blocking");

    // 5. The follow-up runs under the waypoint.
    expect_done(&exercise, &followup)?;
    step("the follow-up squad ran to completion");

    let second_review = wait_for_review(client, &[review.as_str()])?;
    wait_for_status(client, &second_review, "in_review")?;
    merge_review_into_main(client, &fixture, &second_review)?;
    step("the follow-up's own review merged");

    // 6. The follow-up wrote a deferred prophecy of its own; the depth cap
    //    keeps it from chaining into another offer.
    let nested = prophecy_bodies(client, &followup)?;
    expect(
        nested.iter().any(|b| b == NESTED_DEFERRED_NOTE),
        "the follow-up's own deferred prophecy was recorded",
        &format!("{nested:?}"),
    )?;
    expect_no_offer(client, &second_review)?;
    step("a follow-up's own deferrals are not offered again");

    // 7. With the follow-up landed, the waypoint closes.
    wait_for_waypoint_state(client, &waypoint, "closed")?;
    step("the waypoint closed once the follow-up landed");

    let landed = std::fs::read_dir(&fixture.path)
        .map_err(|e| e.to_string())?
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name() == "followup.txt");
    expect(
        landed,
        "the follow-up's change is on the base branch",
        &fixture.path.display().to_string(),
    )?;
    Ok(())
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

fn wait_for_waypoint_state(
    client: &DaemonClient,
    waypoint: &str,
    want: &str,
) -> Result<(), String> {
    poll(&format!("waypoint {waypoint} to be {want}"), || {
        let detail = client.waypoint_get(waypoint).map_err(|e| e.to_string())?;
        Ok((detail["state"] == want).then_some(()))
    })
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
