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
//! remote. Pointing that remote at a `git://` listener that accepts the
//! connection and then simply waits makes the handler take a duration the
//! test picks exactly, with no sleep injected into production code and no
//! network access.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ralphus_daemon::server::serve_with;
use ralphus_daemon::store::Store;

/// How long the fake git remote stalls each connection -- i.e. how long the
/// slow GET takes. Long enough that a serialized accept loop is unmistakable,
/// short enough to keep the test quick.
const STALL: Duration = Duration::from_millis(1500);

/// The budget a request that is *not* the slow one must fit in. Well under
/// [`STALL`], so the assertion fails outright if the request was queued
/// behind it rather than answered alongside it.
const PROMPT: Duration = Duration::from_millis(750);

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

/// Bind a `git://` port that accepts connections and then stalls for
/// [`STALL`] before dropping them, so `git fetch` against it takes a duration
/// this test controls. The listener thread outlives the test on purpose --
/// the test binary exiting is what cleans it up.
fn stalling_git_remote() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalling remote");
    let port = listener.local_addr().expect("local addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            thread::spawn(move || {
                thread::sleep(STALL);
                drop(stream);
            });
        }
    });
    port
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
fn fixture(repos: usize) -> (String, Vec<String>) {
    let port = stalling_git_remote();
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
    (format!("http://{addr}"), pr_ids)
}

fn request(method: &str, url: &str) -> (u16, Duration) {
    let started = Instant::now();
    let resp = match method {
        "POST" => ureq::post(url).send_string(""),
        _ => ureq::get(url).call(),
    };
    let status = match resp {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("request error: {e}"),
    };
    (status, started.elapsed())
}

/// The acceptance criterion: kicking off a rebase is answered promptly even
/// while the board's own polling has a multi-second read in flight.
#[test]
fn merge_kickoff_is_answered_while_a_slow_read_is_in_flight() {
    let (base, pr_ids) = fixture(1);

    let slow_url = format!("{base}/api/pull-requests/{}/sync-status", pr_ids[0]);
    let slow = thread::spawn(move || request("GET", &slow_url));

    // Let the slow read reach its `git fetch` before timing anything, so the
    // accept loop really is occupied when the POST arrives.
    thread::sleep(Duration::from_millis(400));

    // A guardian id that does not exist: this exercises the real merge route
    // and its store access, and returns 404 without starting a rebase whose
    // worker would then outlive the test.
    let (status, elapsed) = request(
        "POST",
        &format!("{base}/api/guardians/guardian-000000009999/merge"),
    );
    assert_eq!(status, 404, "merge route reached and answered");
    assert!(
        elapsed < PROMPT,
        "merge kickoff took {elapsed:?}, which means it queued behind the in-flight read"
    );

    let (slow_status, slow_elapsed) = slow.join().expect("slow read thread");
    assert_eq!(slow_status, 200);
    assert!(
        slow_elapsed >= STALL,
        "the read was supposed to be slow but took only {slow_elapsed:?} -- \
         the fixture is not producing the load this test needs"
    );
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
    let (base, pr_ids) = fixture(READS);

    let started = Instant::now();
    let readers: Vec<_> = pr_ids
        .iter()
        .map(|pr_id| {
            let url = format!("{base}/api/pull-requests/{pr_id}/sync-status");
            thread::spawn(move || request("GET", &url))
        })
        .collect();
    for reader in readers {
        let (status, _) = reader.join().expect("reader thread");
        assert_eq!(status, 200);
    }
    let total = started.elapsed();

    assert!(
        total >= STALL,
        "each read must really have stalled; total was {total:?}"
    );
    assert!(
        total < STALL * 2,
        "{READS} {STALL:?} reads took {total:?} -- they were served one at a time"
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
    let (base, pr_ids) = fixture(1);
    let url = format!("{base}/api/pull-requests/{}/sync-status", pr_ids[0]);

    let started = Instant::now();
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let url = url.clone();
            thread::spawn(move || request("GET", &url))
        })
        .collect();
    for reader in readers {
        let (status, _) = reader.join().expect("reader thread");
        assert_eq!(status, 200);
    }
    let total = started.elapsed();

    assert!(
        total >= STALL * 2,
        "two fetches in one repository took {total:?} -- they overlapped, so each \
         one's `rev-parse FETCH_HEAD` could read the other's fetch"
    );
}
