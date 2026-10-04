//! RAL-565: auto-run of ready manual checks.
//!
//! A check whose `auto_run` resolves to on starts by itself the moment its
//! preparation finishes (its build commands succeeded, or it had none), in the
//! same visible terminal a click would open. This module owns the decisions
//! around that launch -- which checks qualify, the per-client claim that keeps a
//! generation from running twice, which runs are in flight, and the notices and
//! board warnings -- while `server.rs` owns the terminal itself
//! ([`crate::server::run_check_unattended`]).
//!
//! Two properties matter to callers:
//!
//! * The claim is per `(review, check, client, prepared generation)` and is
//!   stored in SQLite. One client's claim never blocks another client's, and a
//!   daemon restart cannot launch a generation a second time.
//! * Nothing here kills a run. A newer rebase, feedback pass or auto-PR fix
//!   only produces a warning ([`note_superseded`]).
//!
//! A failing check is that check's result, not a fault in ralphus: it produces
//! an informational notice ([`NotifiableEventKind::ReviewAutoRunCheckFailed`])
//! with no remediation and leaves the review's state untouched.

use std::sync::Mutex;

use crate::guardian::{GuardianCheck, GuardianStatus, GuardianView};
use crate::mailbox::MailboxPriority;
use crate::monitor::NotifiableEventKind;
use crate::store_lock::StoreHandle;

/// Where one check lives in a review: `kind` is `"action"` for a declared
/// `[[review.action]]` and `"manual"` for a generated check; `index` is its
/// position in that list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckRef {
    pub kind: &'static str,
    pub index: usize,
}

/// What [`plan`] decided for a review right now.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Checks that should start.
    pub runnable: Vec<CheckRef>,
    /// Opted-in, ready checks that cannot start because an input has no value,
    /// with the names of those inputs.
    pub needs_input: Vec<(CheckRef, Vec<String>)>,
}

/// A check whose auto-run is in flight on this daemon.
#[derive(Debug, Clone)]
struct RunningCheck {
    review_id: String,
    check: CheckRef,
    label: String,
}

/// In-flight auto-runs. Process memory by design: a run is a child of this
/// daemon's launch thread, so it cannot outlive the process.
static RUNNING: Mutex<Vec<RunningCheck>> = Mutex::new(Vec::new());

fn running() -> std::sync::MutexGuard<'static, Vec<RunningCheck>> {
    RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Emit one `auto_run` Cartographer row (and its stderr line) for review `id`
/// through the already-held `store` guard.
fn note(
    store: &crate::store::Store,
    id: &str,
    level: crate::logging::LogLevel,
    message: String,
    payload: serde_json::Value,
) {
    crate::cartographer::Note::new("auto_run")
        .level(level)
        .scope("guardian")
        .guardian(id)
        .emit(store, message, payload);
}

fn checks_of<'a>(g: &'a GuardianView, kind: &str) -> &'a [GuardianCheck] {
    if kind == "action" {
        &g.action_hints
    } else {
        &g.manual_commands
    }
}

/// The check's display name for notices: its label, or its 1-based position
/// for a generated check, which has none.
fn display_label(check: &GuardianCheck, at: CheckRef) -> String {
    check
        .label
        .clone()
        .unwrap_or_else(|| format!("Manual check {}", at.index + 1))
}

/// Names of the inputs of `check` that have neither a default nor a value the
/// review already stores.
fn missing_inputs(g: &GuardianView, check: &GuardianCheck) -> Vec<String> {
    check
        .inputs
        .iter()
        .filter(|input| input.default.is_empty() && !g.input_values.contains_key(&input.name))
        .map(|input| input.name.clone())
        .collect()
}

/// Which checks of `g` should start on their own right now. A check qualifies
/// when its own `auto_run` -- falling back to the review's resolved default --
/// is on, it is ready, runnable as a command, local, and the review is not in a
/// terminal state. Remote reviews are out of scope: a check bound for a review
/// machine never auto-runs.
pub(crate) fn plan(g: &GuardianView) -> Plan {
    let mut out = Plan::default();
    if GuardianStatus::is_terminal_status(&g.status) {
        return out;
    }
    for kind in ["action", "manual"] {
        for (index, check) in checks_of(g, kind).iter().enumerate() {
            let eligible = check.auto_run.unwrap_or(g.effective_auto_run)
                && check.preparation_state.as_deref() == Some("ready")
                && check.command.is_some()
                && check.run_on.as_deref() != Some("review_machine")
                && !(check.run_on.is_none() && g.machine.is_some());
            if !eligible {
                continue;
            }
            let at = CheckRef { kind, index };
            let missing = missing_inputs(g, check);
            if missing.is_empty() {
                out.runnable.push(at);
            } else {
                out.needs_input.push((at, missing));
            }
        }
    }
    out
}

/// The generation key a check's auto-run claim is stored under: the instant it
/// was last prepared, so a rebuild opens a new claim while a repeat pass over
/// the same build cannot start it twice.
fn generation_of(check: &GuardianCheck) -> i64 {
    check.prepared_at_ms.unwrap_or(0)
}

/// Claim `check`'s generation for every client that would run it and report
/// whether any claim was new. Each client holds its own slot, so a claim by one
/// never hides the generation from another. A review with no enabled local
/// recipient is claimed under the empty client id.
fn claim(store: &StoreHandle, review_id: &str, at: CheckRef, check: &GuardianCheck) -> bool {
    let guard = store.lock();
    let recipients = guard.guardian_recipients(review_id).unwrap_or_else(|e| {
        note(
            &guard,
            review_id,
            crate::logging::LogLevel::WARNING,
            format!("could not list review clients for auto-run claim review={review_id}: {e}"),
            serde_json::json!({ "error": e.to_string() }),
        );
        Vec::new()
    });
    let mut clients: Vec<String> = recipients
        .into_iter()
        .filter(|client| client.enabled && client.local)
        .map(|client| client.id)
        .collect();
    if clients.is_empty() {
        clients.push(String::new());
    }
    let key = format!("{}-{}", at.kind, at.index);
    let generation = generation_of(check);
    let mut any_new = false;
    for client in &clients {
        match guard.claim_auto_run(review_id, &key, client, generation) {
            Ok(true) => any_new = true,
            Ok(false) => {}
            Err(e) => note(
                &guard,
                review_id,
                crate::logging::LogLevel::WARNING,
                format!("could not claim auto-run review={review_id} check={key}: {e}"),
                serde_json::json!({ "check": key, "client": client, "error": e.to_string() }),
            ),
        }
    }
    any_new
}

/// Start every ready, opted-in check of review `id`, once per client and
/// prepared generation. Called from the preparation worker each time a check
/// becomes ready; launching happens on its own thread so that worker -- which
/// holds the review's preparation gate -- is never held up by a check.
pub(crate) fn run_ready_checks(store: &StoreHandle, id: &str) {
    let g = {
        let guard = store.lock();
        match guard.get_guardian(id) {
            Ok(g) => g,
            Err(e) => {
                note(
                    &guard,
                    id,
                    crate::logging::LogLevel::WARNING,
                    format!("auto-run skipped: could not load review {id}: {e}"),
                    serde_json::json!({ "error": e.to_string() }),
                );
                return;
            }
        }
    };
    let plan = plan(&g);
    for (at, missing) in &plan.needs_input {
        let note_text = format!(
            "Auto-run skipped: no value for input{} {}. Fill in the value and run this check by hand.",
            if missing.len() == 1 { "" } else { "s" },
            missing.join(", ")
        );
        let guard = store.lock();
        let saved = guard.set_check_auto_run_note(id, at.kind, at.index, Some(&note_text));
        note(
            &guard,
            id,
            if saved.is_ok() {
                crate::logging::LogLevel::INFO
            } else {
                crate::logging::LogLevel::WARNING
            },
            format!(
                "auto-run skipped review={id} check={}-{}: missing input value(s) {}",
                at.kind,
                at.index,
                missing.join(", ")
            ),
            serde_json::json!({
                "check": format!("{}-{}", at.kind, at.index),
                "missing": missing,
                "note_saved": saved.is_ok(),
                "error": saved.err().map(|e| e.to_string()),
            }),
        );
    }
    for at in plan.runnable {
        let check = checks_of(&g, at.kind)[at.index].clone();
        if !claim(store, id, at, &check) {
            continue;
        }
        {
            let guard = store.lock();
            if check.auto_run_note.is_some() {
                if let Err(e) = guard.set_check_auto_run_note(id, at.kind, at.index, None) {
                    note(
                        &guard,
                        id,
                        crate::logging::LogLevel::WARNING,
                        format!(
                            "could not clear stale auto-run note review={id} check={}-{}: {e}",
                            at.kind, at.index
                        ),
                        serde_json::json!({ "error": e.to_string() }),
                    );
                }
            }
            note(
                &guard,
                id,
                crate::logging::LogLevel::INFO,
                format!(
                    "auto-run triggered review={id} check={}-{} label={:?}",
                    at.kind,
                    at.index,
                    display_label(&check, at)
                ),
                serde_json::json!({
                    "check": format!("{}-{}", at.kind, at.index),
                    "generation": generation_of(&check),
                }),
            );
        }
        let store = store.clone();
        let g = g.clone();
        let id = id.to_string();
        std::thread::spawn(move || run_one(&store, &id, &g, &check, at));
    }
}

/// Run one claimed check to completion and report a failure.
fn run_one(store: &StoreHandle, id: &str, g: &GuardianView, check: &GuardianCheck, at: CheckRef) {
    let label = display_label(check, at);
    running().push(RunningCheck {
        review_id: id.to_string(),
        check: at,
        label: label.clone(),
    });
    let outcome = crate::server::run_check_unattended(store, g, check, at.kind, at.index);
    running().retain(|r| !(r.review_id == id && r.check == at));
    let detail = match outcome {
        Some(Some(0)) => {
            note(
                &store.lock(),
                id,
                crate::logging::LogLevel::INFO,
                format!(
                    "auto-run passed review={id} check={}-{} label={label:?}",
                    at.kind, at.index
                ),
                serde_json::json!({ "check": format!("{}-{}", at.kind, at.index) }),
            );
            return;
        }
        None => {
            note(
                &store.lock(),
                id,
                crate::logging::LogLevel::WARNING,
                format!(
                    "auto-run did not start review={id} check={}-{} label={label:?}: no \
                     terminal could be opened",
                    at.kind, at.index
                ),
                serde_json::json!({ "check": format!("{}-{}", at.kind, at.index) }),
            );
            return;
        }
        Some(Some(code)) => format!("exited with code {code}"),
        Some(None) => "timed out without reporting an exit code".to_string(),
    };
    let guard = store.lock();
    note(
        &guard,
        id,
        crate::logging::LogLevel::WARNING,
        format!(
            "auto-run failed review={id} check={}-{} label={label:?}: {detail}",
            at.kind, at.index
        ),
        serde_json::json!({ "check": format!("{}-{}", at.kind, at.index), "detail": detail }),
    );
    let notified = guard.notify_watchers(
        NotifiableEventKind::ReviewAutoRunCheckFailed,
        &format!("guardian:{id}"),
        MailboxPriority::Normal,
        &format!(
            "Review \"{}\": the \"{label}\" Manual Check {detail} when it auto-ran. Its output is on the board.",
            g.name
        ),
        g.squad_id.as_deref(),
    );
    if let Err(e) = notified {
        note(
            &guard,
            id,
            crate::logging::LogLevel::WARNING,
            format!("could not notify watchers of failed auto-run review={id}: {e}"),
            serde_json::json!({ "error": e.to_string() }),
        );
    }
}

/// Warn about every auto-run still in flight for review `id` because a newer
/// `trigger` (`"rebase"`, `"feedback"` or `"auto-PR fix"`) arrived. The run is
/// left alone; its check gets a board warning and watchers get a notice.
pub(crate) fn note_superseded(store: &StoreHandle, id: &str, trigger: &str) {
    let in_flight: Vec<RunningCheck> = running()
        .iter()
        .filter(|r| r.review_id == id)
        .cloned()
        .collect();
    if in_flight.is_empty() {
        return;
    }
    let g = {
        let guard = store.lock();
        match guard.get_guardian(id) {
            Ok(g) => g,
            Err(e) => {
                note(
                    &guard,
                    id,
                    crate::logging::LogLevel::WARNING,
                    format!(
                        "could not load review {id} to warn {} superseded auto-run(s): {e}",
                        in_flight.len()
                    ),
                    serde_json::json!({ "trigger": trigger, "error": e.to_string() }),
                );
                return;
            }
        }
    };
    for r in in_flight {
        let message = superseded_message(trigger, &r.label);
        let guard = store.lock();
        let saved = guard.set_check_auto_run_note(id, r.check.kind, r.check.index, Some(&message));
        let notified = guard.notify_watchers(
            NotifiableEventKind::ReviewAutoRunSuperseded,
            &format!("guardian:{id}"),
            MailboxPriority::Normal,
            &format!("Review \"{}\": {message}", g.name),
            g.squad_id.as_deref(),
        );
        let ok = saved.is_ok() && notified.is_ok();
        note(
            &guard,
            id,
            if ok {
                crate::logging::LogLevel::INFO
            } else {
                crate::logging::LogLevel::WARNING
            },
            format!(
                "in-flight auto-run superseded by {trigger} review={id} check={}-{} label={:?}",
                r.check.kind, r.check.index, r.label
            ),
            serde_json::json!({
                "trigger": trigger,
                "check": format!("{}-{}", r.check.kind, r.check.index),
                "note_error": saved.err().map(|e| e.to_string()),
                "notify_error": notified.err().map(|e| e.to_string()),
            }),
        );
    }
}

fn superseded_message(trigger: &str, label: &str) -> String {
    format!(
        "New {trigger} arrived while you were running the \"{label}\" Manual Check. \
         Your check environment may be out of date. Consider closing and re-running."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::{CheckInput, CheckInputType};
    use crate::store::Store;

    fn check(label: &str, auto_run: Option<bool>, state: &str) -> GuardianCheck {
        GuardianCheck {
            label: Some(label.to_string()),
            command: Some("echo hi".to_string()),
            auto_run,
            preparation_state: Some(state.to_string()),
            ..GuardianCheck::default()
        }
    }

    fn input(name: &str, default: &str) -> CheckInput {
        CheckInput {
            name: name.to_string(),
            message: String::new(),
            default: default.to_string(),
            r#type: CheckInputType::String,
        }
    }

    fn view(review_default: Option<bool>, hints: &[GuardianCheck]) -> GuardianView {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store.set_guardian_auto_run(&id, review_default).unwrap();
        store.set_guardian_action_hints(&id, hints).unwrap();
        store.get_guardian(&id).unwrap()
    }

    fn action(index: usize) -> CheckRef {
        CheckRef {
            kind: "action",
            index,
        }
    }

    #[test]
    fn off_by_default_and_the_check_setting_beats_the_review_default() {
        let hints = [
            check("inherit", None, "ready"),
            check("on", Some(true), "ready"),
            check("off", Some(false), "ready"),
        ];
        assert_eq!(
            plan(&view(None, &hints)).runnable,
            vec![action(1)],
            "unset everywhere is off; only the explicit opt-in runs"
        );
        assert_eq!(
            plan(&view(Some(true), &hints)).runnable,
            vec![action(0), action(1)],
            "a review default turns inheriting checks on; an opted-out check stays manual"
        );
        assert!(
            plan(&view(Some(false), &hints[..1])).runnable.is_empty(),
            "a review default of off keeps an inheriting check manual"
        );
    }

    #[test]
    fn a_check_with_no_build_commands_qualifies_once_it_is_ready() {
        let hints = [check("quick", Some(true), "ready")];
        assert!(hints[0].prepare.is_empty());
        assert_eq!(plan(&view(None, &hints)).runnable, vec![action(0)]);
    }

    #[test]
    fn only_ready_checks_qualify() {
        let hints = [
            check("waiting", Some(true), "waiting"),
            check("preparing", Some(true), "preparing"),
            check("failed", Some(true), "failed"),
            check("stale", Some(true), "stale"),
            check("ready", Some(true), "ready"),
        ];
        assert_eq!(plan(&view(None, &hints)).runnable, vec![action(4)]);
    }

    #[test]
    fn a_check_with_an_unfilled_input_is_reported_instead_of_run() {
        let mut needs = check("needs", Some(true), "ready");
        needs.inputs = vec![input("port", ""), input("host", "localhost")];
        let mut defaulted = check("defaulted", Some(true), "ready");
        defaulted.inputs = vec![input("port", "8080")];
        let planned = plan(&view(None, &[needs, defaulted]));
        assert_eq!(planned.runnable, vec![action(1)]);
        assert_eq!(
            planned.needs_input,
            vec![(action(0), vec!["port".to_string()])]
        );
    }

    #[test]
    fn a_check_for_the_review_machine_never_auto_runs() {
        let mut remote = check("remote", Some(true), "ready");
        remote.run_on = Some("review_machine".to_string());
        let local = check("local", Some(true), "ready");
        assert_eq!(
            plan(&view(None, &[remote, local])).runnable,
            vec![action(1)]
        );
    }

    #[test]
    fn the_claim_is_once_per_client_and_generation_and_clients_do_not_block_each_other() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        assert!(store.claim_auto_run(&id, "action-0", "alice", 100).unwrap());
        assert!(
            !store.claim_auto_run(&id, "action-0", "alice", 100).unwrap(),
            "the same client cannot take the same generation twice"
        );
        assert!(
            store.claim_auto_run(&id, "action-0", "bob", 100).unwrap(),
            "another client's claim does not block this one"
        );
        assert!(
            store.claim_auto_run(&id, "action-0", "alice", 200).unwrap(),
            "a re-prepared generation is a new claim"
        );
        assert!(
            store.claim_auto_run(&id, "action-1", "alice", 100).unwrap(),
            "each check has its own slot"
        );
    }

    #[test]
    fn the_claim_is_durable_across_a_reopened_store() {
        let dir =
            std::env::temp_dir().join(format!("ralphus-auto-run-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.db");
        let id = {
            let store = Store::open(&path).unwrap();
            let id = store.create_guardian("r", "main", "/repo").unwrap();
            assert!(store.claim_auto_run(&id, "action-0", "", 7).unwrap());
            id
        };
        let reopened = Store::open(&path).unwrap();
        assert!(
            !reopened.claim_auto_run(&id, "action-0", "", 7).unwrap(),
            "a restart must not re-arm a claimed generation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_superseded_warning_names_the_trigger_and_the_check() {
        assert_eq!(
            superseded_message("rebase", "Smoke"),
            "New rebase arrived while you were running the \"Smoke\" Manual Check. \
             Your check environment may be out of date. Consider closing and re-running."
        );
    }

    #[test]
    fn a_note_can_be_set_and_cleared_on_one_check() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .set_guardian_action_hints(&id, &[check("a", None, "ready"), check("b", None, "ready")])
            .unwrap();
        store
            .set_check_auto_run_note(&id, "action", 1, Some("careful"))
            .unwrap();
        let g = store.get_guardian(&id).unwrap();
        assert_eq!(g.action_hints[0].auto_run_note, None);
        assert_eq!(g.action_hints[1].auto_run_note.as_deref(), Some("careful"));
        store
            .set_check_auto_run_note(&id, "action", 1, None)
            .unwrap();
        assert_eq!(
            store.get_guardian(&id).unwrap().action_hints[1].auto_run_note,
            None
        );
    }
}
