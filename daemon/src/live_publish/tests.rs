use super::*;
use crate::live_actions::NewLiveAction;
use crate::live_results::SECTION_START;
use crate::live_runs::LiveRunKind;
use crate::mock_forge::MockForge;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type Bodies = Arc<Mutex<BTreeMap<i64, String>>>;
/// `(method, pr number, keys of the JSON payload)` for every write.
type WriteRecord = (String, i64, Vec<String>);
type Writes = Arc<Mutex<Vec<WriteRecord>>>;

struct Fixture {
    store: StoreHandle,
    gid: String,
    branch_ids: Vec<String>,
    bodies: Bodies,
    writes: Writes,
    fail: Arc<AtomicBool>,
    mock: MockForge<()>,
    root: std::path::PathBuf,
}

fn git(root: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn fixture(forge: &'static str) -> Fixture {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let bodies: Bodies = Arc::default();
    let writes: Writes = Arc::default();
    let fail = Arc::new(AtomicBool::new(false));
    let (b, w, f) = (bodies.clone(), writes.clone(), fail.clone());
    let mock = MockForge::start(move |server| {
        for mut req in server.requests() {
            let number: i64 = req
                .url()
                .rsplit('/')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if f.load(Ordering::SeqCst) {
                req.respond(tiny_http::Response::from_string("{}").with_status_code(500))
                    .unwrap();
                continue;
            }
            let field = if forge == "github" {
                "body"
            } else {
                "description"
            };
            if req.method() == &tiny_http::Method::Get {
                let text = b.lock().unwrap().get(&number).cloned().unwrap_or_default();
                let payload = serde_json::json!({ field: text });
                req.respond(tiny_http::Response::from_string(payload.to_string()))
                    .unwrap();
                continue;
            }
            let mut raw = String::new();
            req.as_reader().read_to_string(&mut raw).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let keys = payload.as_object().unwrap().keys().cloned().collect();
            w.lock()
                .unwrap()
                .push((req.method().to_string(), number, keys));
            b.lock()
                .unwrap()
                .insert(number, payload[field].as_str().unwrap().to_string());
            req.respond(tiny_http::Response::from_string("{}")).unwrap();
        }
    });
    let addr = mock.addr();
    let n = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ralphus-live-publish-{forge}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init"]);
    let remote = if forge == "github" {
        "https://github.com/acme/widget.git"
    } else {
        "https://gitlab.com/acme/widget.git"
    };
    git(&root, &["remote", "add", "origin", remote]);
    std::fs::write(
        root.join(".ralphus.toml"),
        format!(
            "[forge]\nkind = \"{forge}\"\napi_base = \"http://{addr}\"\ntoken_env = \"RALPHUS_TEST_FORGE_TOKEN\"\n"
        ),
    )
    .unwrap();
    let store: StoreHandle = Arc::new(crate::store_lock::StoreMutex::new(
        crate::store::Store::open_in_memory().unwrap(),
    ));
    let gid = store
        .lock()
        .create_guardian("demo", "main", root.to_str().unwrap())
        .unwrap();
    for branch in ["a", "b"] {
        store.lock().add_guardian_branch(&gid, branch).unwrap();
    }
    let branch_ids: Vec<String> = store
        .lock()
        .get_guardian(&gid)
        .unwrap()
        .branches
        .into_iter()
        .map(|b| b.id)
        .collect();
    for (i, alias) in ["a-alias", "b-alias"].into_iter().enumerate() {
        let number = i64::try_from(i).unwrap() + 1;
        store
            .lock()
            .create_pull_request(
                &gid,
                Some(&branch_ids[i]),
                forge,
                "acme/widget",
                alias,
                if i == 0 { "main" } else { "a-alias" },
                alias,
                "",
                Some(number),
                None,
            )
            .unwrap();
        bodies
            .lock()
            .unwrap()
            .insert(number, format!("Original description of #{number}"));
    }
    Fixture {
        store,
        gid,
        branch_ids,
        bodies,
        writes,
        fail,
        mock,
        root,
    }
}

fn add_actions(fx: &Fixture) {
    let action = |label: &str, ids: Vec<String>| NewLiveAction {
        label: label.to_string(),
        prompt: format!("do {label}"),
        source_cells: Vec::new(),
        branch_ids: ids,
        attribution_error: None,
    };
    fx.store
        .lock()
        .insert_guardian_live_actions(
            &fx.gid,
            &[
                action("Alpha", vec![fx.branch_ids[0].clone()]),
                action("Beta", fx.branch_ids.clone()),
                action("Gamma", Vec::new()),
            ],
        )
        .unwrap();
}

fn add_run(fx: &Fixture, kind: LiveRunKind, idx: i64, label: &str, n: i64) {
    let guard = fx.store.lock();
    let run_id = guard
        .queue_live_run(&fx.gid, kind, idx, label, "manual")
        .unwrap();
    guard
        .conn
        .execute(
            "UPDATE guardian_live_runs SET state='finished', prompt=?, verdict='failed',
                 queued_at_ms=?, started_at_ms=?, finished_at_ms=? WHERE run_id=?",
            rusqlite::params![
                format!("prompt-{label}-{n}"),
                1_000 + n,
                1_760_617_800_000_i64,
                1_760_617_860_000_i64,
                run_id
            ],
        )
        .unwrap();
}

fn descriptions(fx: &Fixture) -> Vec<String> {
    fx.store
        .lock()
        .list_pull_requests_for_guardian(&fx.gid)
        .unwrap()
        .into_iter()
        .map(|p| p.description)
        .collect()
}

fn finish(fx: Fixture) -> (BTreeMap<i64, String>, Vec<WriteRecord>) {
    let bodies = fx.bodies.lock().unwrap().clone();
    let writes = fx.writes.lock().unwrap().clone();
    fx.mock.finish();
    let _ = std::fs::remove_dir_all(&fx.root);
    (bodies, writes)
}

fn publishes_per_pr(forge: &'static str) {
    let fx = fixture(forge);
    add_actions(&fx);
    for n in 1..=4 {
        add_run(&fx, LiveRunKind::Live, 0, "Alpha", n);
    }
    add_run(&fx, LiveRunKind::Live, 1, "Beta", 1);
    add_run(&fx, LiveRunKind::Live, 2, "Gamma", 1);
    add_run(&fx, LiveRunKind::Action, 0, "UserRun", 1);

    request_refresh(&fx.store, &fx.gid);
    process_due_refreshes(&fx.store);
    let first = fx.bodies.lock().unwrap().clone();
    assert!(
        fx.store
            .lock()
            .live_result_publish_status(&fx.gid)
            .unwrap()
            .is_empty()
    );

    // Branch a: its own action and the multi-branch one, not the other
    // branch's or the review-wide one.
    let a = &first[&1];
    assert!(a.starts_with("Original description of #1"), "{a}");
    assert!(a.contains("Alpha") && a.contains("Beta"), "{a}");
    assert!(!a.contains("Gamma"), "{a}");
    // Only the newest three runs of an action.
    assert!(
        a.contains("prompt-Alpha-4") && a.contains("prompt-Alpha-2"),
        "{a}"
    );
    assert!(!a.contains("prompt-Alpha-1"), "{a}");
    // A user-authored action run is not a live-test result.
    assert!(!a.contains("UserRun"), "{a}");
    // Branch b (the top PR): the multi-branch action runs once but shows
    // here too, plus the review-wide entry.
    let b = &first[&2];
    assert!(b.contains("Beta") && b.contains("Gamma"), "{b}");
    assert!(!b.contains("Alpha"), "{b}");
    assert_eq!(a.matches(SECTION_START).count(), 1);
    assert_eq!(b.matches(SECTION_START).count(), 1);
    assert_eq!(descriptions(&fx), [a.clone(), b.clone()]);

    // A second request changes nothing and writes nothing.
    let writes_before = fx.writes.lock().unwrap().len();
    request_refresh(&fx.store, &fx.gid);
    process_due_refreshes(&fx.store);
    assert_eq!(fx.writes.lock().unwrap().len(), writes_before);
    assert_eq!(*fx.bodies.lock().unwrap(), first);

    // The stack's structure is untouched.
    let bases: Vec<String> = fx
        .store
        .lock()
        .list_pull_requests_for_guardian(&fx.gid)
        .unwrap()
        .into_iter()
        .map(|p| p.base_ref)
        .collect();
    assert_eq!(bases, ["main", "a-alias"]);
    let (_, writes) = finish(fx);
    assert_eq!(writes.len(), 2);
    let key = if forge == "github" {
        "body"
    } else {
        "description"
    };
    for (_, _, keys) in writes {
        assert_eq!(keys, [key]);
    }
}

#[test]
fn github_descriptions_get_each_prs_actions_once() {
    publishes_per_pr("github");
}

#[test]
fn gitlab_descriptions_get_each_mrs_actions_once() {
    publishes_per_pr("gitlab");
}

#[test]
fn a_failed_update_is_queued_backed_off_and_retried_by_hand() {
    let fx = fixture("github");
    add_actions(&fx);
    add_run(&fx, LiveRunKind::Live, 0, "Alpha", 1);
    fx.fail.store(true, Ordering::SeqCst);

    request_refresh(&fx.store, &fx.gid);
    process_due_refreshes(&fx.store);
    let pending = fx.store.lock().live_result_publish_status(&fx.gid).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending
            .iter()
            .all(|p| p.attempts == 1 && p.last_error.is_some())
    );
    assert!(
        pending
            .iter()
            .all(|p| p.next_attempt_ms > crate::store::now_ms())
    );
    // The confirmed body is only stored after the forge accepts a write.
    assert_eq!(descriptions(&fx), ["", ""]);

    // Not due yet: another pass leaves the attempt count alone.
    process_due_refreshes(&fx.store);
    assert_eq!(
        fx.store.lock().live_result_publish_status(&fx.gid).unwrap(),
        pending
    );

    // The manual retry skips the delay and, with the forge back, publishes.
    fx.fail.store(false, Ordering::SeqCst);
    retry_now(&fx.store, &fx.gid);
    assert!(
        fx.store
            .lock()
            .live_result_publish_status(&fx.gid)
            .unwrap()
            .is_empty()
    );
    let (bodies, _) = finish(fx);
    assert!(bodies[&1].contains("Alpha"));
}

#[test]
fn the_retry_queue_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!("ralphus-live-queue-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("state.db");
    let (gid, pr_id) = {
        let store = crate::store::Store::open(&db).unwrap();
        let gid = store.create_guardian("demo", "main", "C:/x").unwrap();
        let pr_id = store
            .create_pull_request(
                &gid,
                None,
                "github",
                "acme/widget",
                "x",
                "main",
                "x",
                "",
                Some(7),
                None,
            )
            .unwrap();
        store.enqueue_live_result_refresh(&gid).unwrap();
        store.fail_live_result_refresh(&pr_id, 3, "boom");
        (gid, pr_id)
    };
    let store = crate::store::Store::open(&db).unwrap();
    let pending = store.live_result_publish_status(&gid).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].pr_id, pr_id);
    assert_eq!(pending[0].attempts, 3);
    assert_eq!(pending[0].last_error.as_deref(), Some("boom"));
    assert!(
        store
            .due_live_result_refreshes(crate::store::now_ms())
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn backoff_doubles_and_is_capped() {
    assert_eq!(backoff_ms(1), 30_000);
    assert_eq!(backoff_ms(2), 60_000);
    assert_eq!(backoff_ms(3), 120_000);
    assert_eq!(backoff_ms(50), BACKOFF_CAP_MS);
}

#[test]
fn a_legacy_action_is_resolved_once_and_the_failure_is_kept() {
    let fx = fixture("github");
    fx.store
        .lock()
        .conn
        .execute(
            "INSERT INTO guardian_live_actions
                (guardian_id, idx, label, prompt, source_cells, created_at_ms)
             VALUES (?, 0, 'Old', 'p', '[\"cell:nope:0:0\"]', 1)",
            rusqlite::params![fx.gid],
        )
        .unwrap();
    assert_eq!(
        fx.store
            .lock()
            .resolve_legacy_live_actions(&fx.gid)
            .unwrap(),
        1
    );
    let actions = fx.store.lock().guardian_live_actions(&fx.gid).unwrap();
    assert!(actions[0].branch_ids.is_empty());
    assert!(actions[0].attribution_error.is_some());
    assert_eq!(
        fx.store
            .lock()
            .resolve_legacy_live_actions(&fx.gid)
            .unwrap(),
        0
    );
    finish(fx);
}
