//! The daemon answers a request while a slow one is still in flight.
//!
//! `tiny_http` is a synchronous, one-request-at-a-time server, so the accept
//! loop is the whole API's critical path: whatever handler it is currently
//! inside, every other connection waits behind. Several board endpoints
//! legitimately take seconds because they shell out to git or call a forge
//! over the network, and the Reviews tab polls them on every refresh -- which
//! is enough to make a `POST /api/guardians/{id}/merge` (a DB state
//! transition plus a thread spawn) sit unanswered for as long as that poll
//! takes. `server::ReadPool` is what stops that; these tests are its cover.
//!
//! The slow endpoint here is the real one: `GET
//! /api/pull-requests/{id}/sync-status` runs `git fetch` against the review's
//! remote. Pointing that remote at a `git://` listener that holds accepted
//! connections lets each test observe which requests reached the remote before
//! releasing them, with no sleep injected into production code and no network
//! access.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use ralphus_daemon::server::serve_with;
use ralphus_daemon::store::Store;

/// A generous deadlock backstop. Concurrency is proved by observing which
/// connections reach the fake remote before it is released, not by comparing
/// handler wall-clock time against this budget.
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct StallState {
    connections: usize,
    released: bool,
}

#[derive(Default)]
struct StallController {
    state: Mutex<StallState>,
    changed: Condvar,
}

impl StallController {
    fn connection_arrived(&self) {
        let mut state = self.state.lock().expect("stall state mutex poisoned");
        state.connections += 1;
        self.changed.notify_all();
        while !state.released {
            state = self
                .changed
                .wait(state)
                .expect("stall state mutex poisoned");
        }
    }

    fn wait_for_connections(&self, expected: usize, timeout: Duration) -> bool {
        let state = self.state.lock().expect("stall state mutex poisoned");
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| state.connections < expected)
            .expect("stall state mutex poisoned");
        state.connections >= expected
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("stall state mutex poisoned");
        state.released = true;
        self.changed.notify_all();
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "ralphus")
        .env("GIT_AUTHOR_EMAIL", "ralphus@example.com")
        .env("GIT_COMMITTER_NAME", "ralphus")
        .env("GIT_COMMITTER_EMAIL", "ralphus@example.com")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn tmp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ralphus-http-conc-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Bind a `git://` port that accepts connections and holds them until the test
/// releases them. The controller lets tests prove overlap from observable
/// ordering rather than scheduler-sensitive elapsed-time thresholds.
fn stalling_git_remote() -> (u16, Arc<StallController>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalling remote");
    let port = listener.local_addr().expect("local addr").port();
    let controller = Arc::new(StallController::default());
    let controller_for_listener = Arc::clone(&controller);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let controller = Arc::clone(&controller_for_listener);
            thread::spawn(move || {
                controller.connection_arrived();
                drop(stream);
            });
        }
    });
    (port, controller)
}

/// A live daemon serving `repos` pull requests, **each in its own git
/// repository**, all pointing at a stalling remote.
///
/// One repository per PR is load-bearing, not incidental: `compute_sync_status`
/// serializes the `git fetch` + `rev-parse FETCH_HEAD` pair per repository root
/// (`pr::SYNC_FETCH_LOCKS`), because `FETCH_HEAD` is one file shared by the
/// whole repository and two fetches racing in it can read each other's result.
/// Two sync-status reads of the *same* root are therefore supposed to queue --
/// so measuring the read pool's concurrency needs separate roots, or it just
/// re-measures that lock. `same_repo_sync_status_reads_serialize` below pins
/// the other half of this.
///
/// Returns the daemon's base URL and one PR id per repository.
fn fixture(repos: usize) -> (String, Vec<String>, Arc<StallController>) {
    let (port, stall) = stalling_git_remote();
    let store = Store::open_in_memory().expect("store");
    let mut pr_ids = Vec::with_capacity(repos);

    for i in 0..repos {
        let root = tmp_dir(&format!("repo{i}"));
        git(&root, &["init", "-b", "main"]);
        std::fs::write(root.join("base.txt"), "base\n").expect("write");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        git(&root, &["branch", "review-branch"]);
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                &format!("git://127.0.0.1:{port}/stalls-forever"),
            ],
        );

        let gid = store
            .create_guardian("latency", "main", root.to_str().expect("utf-8 root"))
            .expect("create guardian");
        store
            .add_guardian_branch(&gid, "review-branch")
            .expect("add branch");
        let branch_id = store.get_guardian(&gid).expect("get guardian").branches[0]
            .id
            .clone();
        store
            .set_branch_review(
                &gid,
                &branch_id,
                "review-branch",
                root.join("wt").to_str().expect("utf-8 worktree"),
            )
            .expect("set branch review");
        pr_ids.push(
            store
                .create_pull_request(
                    &gid,
                    Some(&branch_id),
                    "github",
                    "acme/widget",
                    "pr-y",
                    "main",
                    "T",
                    "D",
                    None,
                    None,
                )
                .expect("create pull request"),
        );
    }

    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
    let addr = server.server_addr().to_ip().expect("ip addr");
    thread::spawn(move || serve_with(server, store, 4));
    (format!("http://{addr}"), pr_ids, stall)
}

fn request(method: &str, url: &str) -> u16 {
    let resp = match method {
        "POST" => ureq::post(url).send_string(""),
        _ => ureq::get(url).call(),
    };
    match resp {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("request error: {e}"),
    }
}

/// The acceptance criterion: kicking off a rebase is answered before the
/// board's in-flight polling read is released.
#[test]
fn merge_kickoff_is_answered_while_a_slow_read_is_in_flight() {
    let (base, pr_ids, stall) = fixture(1);

    let slow_url = format!("{base}/api/pull-requests/{}/sync-status", pr_ids[0]);
    let slow = thread::spawn(move || request("GET", &slow_url));

    assert!(
        stall.wait_for_connections(1, WAIT_TIMEOUT),
        "slow read never reached the fake git remote"
    );

    // A guardian id that does not exist: this exercises the real merge route
    // and its store access, and returns 404 without starting a rebase whose
    // worker would then outlive the test.
    let (status_tx, status_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let status = request(
            "POST",
            &format!("{base}/api/guardians/guardian-000000009999/merge"),
        );
        let _ = status_tx.send(status);
    });
    let status = status_rx.recv_timeout(WAIT_TIMEOUT);
    stall.release();
    let status = status.expect(
        "merge kickoff did not answer while the slow read was held; it queued behind the read",
    );
    assert_eq!(status, 404, "merge route reached and answered");

    let slow_status = slow.join().expect("slow read thread");
    assert_eq!(slow_status, 200);
}

/// Reads are answered concurrently with each other, not one after another:
/// the board issues several independent GETs per refresh, and serializing them
/// makes every one of them wait for the slowest.
///
/// Each read targets its own repository so the only thing under test is the
/// read pool -- see [`fixture`] for why sharing a root would instead measure
/// the per-repository fetch lock.
#[test]
fn concurrent_slow_reads_overlap_instead_of_queueing() {
    const READS: usize = 3;
    let (base, pr_ids, stall) = fixture(READS);

    let readers: Vec<_> = pr_ids
        .iter()
        .map(|pr_id| {
            let url = format!("{base}/api/pull-requests/{pr_id}/sync-status");
            thread::spawn(move || request("GET", &url))
        })
        .collect();
    let overlapped = stall.wait_for_connections(READS, WAIT_TIMEOUT);
    stall.release();
    for reader in readers {
        let status = reader.join().expect("reader thread");
        assert_eq!(status, 200);
    }
    assert!(
        overlapped,
        "fewer than {READS} reads reached the fake remote together; they were served one at a time"
    );
}

/// The counterpart to the test above: two sync-status reads of the *same*
/// repository deliberately do NOT overlap.
///
/// `compute_sync_status` fetches and then reads back `FETCH_HEAD`, a single
/// file per repository. Letting two of those interleave would let one PR's
/// `rev-parse` observe the other's fetch and report drift against a sibling
/// PR's tip. Serving reads on a pool is what makes that pairing reachable in
/// the first place, so the lock preventing it is pinned here.
#[test]
fn same_repo_sync_status_reads_serialize() {
    let (base, pr_ids, stall) = fixture(1);
    let url = format!("{base}/api/pull-requests/{}/sync-status", pr_ids[0]);

    let readers: Vec<_> = (0..2)
        .map(|_| {
            let url = url.clone();
            thread::spawn(move || request("GET", &url))
        })
        .collect();
    assert!(
        stall.wait_for_connections(1, WAIT_TIMEOUT),
        "first read never reached the fake git remote"
    );
    assert!(
        !stall.wait_for_connections(2, Duration::from_secs(1)),
        "both same-repository fetches reached the remote together"
    );
    stall.release();
    for reader in readers {
        let status = reader.join().expect("reader thread");
        assert_eq!(status, 200);
    }
}
