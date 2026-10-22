//! The machine-side `exec` lifecycle with the real runner binary: a job is
//! started detached under its supervisor, reaches `done` with the runner's
//! result, sees its environment without that environment touching the job
//! directory, and can be cancelled.

use std::path::Path;
use std::time::{Duration, Instant};

use ralphus_runner::machine::{Context, execute};
use serde_json::{Value, json};

fn context(root: &Path) -> Context {
    Context {
        remote_root: root.to_string_lossy().replace('\\', "/"),
        runner: vec![env!("CARGO_BIN_EXE_ralphus-runner").to_string()],
    }
}

fn spec(root: &Path, cell: &str, command: &str) -> Value {
    let (shell, command) = if cfg!(windows) {
        ("cmd", command.to_string())
    } else {
        ("sh", command.to_string())
    };
    json!({
        "squad_id": "squad-machine-jobs",
        "task": "t",
        "cell_id": cell,
        "cwd": root.to_string_lossy().replace('\\', "/"),
        "command": command,
        "shell": shell,
        "mode": "raw",
        "execution_environment": {"RALPHUS_JOB_SECRET": "job-secret-value-7f3a"},
    })
}

fn wait_terminal(context: &Context, handle: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let reply = execute("status", &json!({"handle": handle}), context);
        assert_eq!(reply.body["ok"], true, "{}", reply.body);
        if matches!(reply.body["state"].as_str(), Some("done" | "failed")) {
            return reply.body;
        }
        assert!(
            Instant::now() < deadline,
            "job did not finish: {}",
            reply.body
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn files_containing(dir: &Path, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if std::fs::read(&path)
                .map(|b| String::from_utf8_lossy(&b).contains(needle))
                .unwrap_or(false)
            {
                found.push(path.display().to_string());
            }
        }
    }
    found
}

#[test]
fn a_job_runs_detached_to_done_with_its_environment_kept_off_disk() {
    let dir = ralphus_test_support::TestDir::new("machine-job-run");
    let context = context(&dir);
    let command = if cfg!(windows) {
        "if defined RALPHUS_JOB_SECRET (echo secret-seen) else (exit 7)"
    } else {
        "test -n \"$RALPHUS_JOB_SECRET\" && echo secret-seen"
    };
    let started = execute("exec", &spec(&dir, "cell-run", command), &context);
    assert_eq!(started.body["ok"], true, "{}", started.body);
    let handle = started.body["handle"]
        .as_str()
        .expect("a handle")
        .to_string();

    let again = execute("exec", &spec(&dir, "cell-run", command), &context);
    let finished = wait_terminal(&context, &handle);
    assert_eq!(finished["state"], "done", "{finished}");
    assert_eq!(finished["result"]["status"], "done", "{finished}");
    // A repeat while the first was alive reattached; one after it finished
    // would have started anew -- either way it named a valid job.
    assert_eq!(again.body["ok"], true, "{}", again.body);

    let streamed = execute("stream", &json!({"handle": handle, "since": 0}), &context);
    assert!(
        streamed.body["output"]
            .as_str()
            .unwrap_or_default()
            .contains("secret-seen")
            || finished["result"].to_string().contains("secret-seen"),
        "{} / {finished}",
        streamed.body
    );
    assert!(
        files_containing(&dir, "job-secret-value-7f3a").is_empty(),
        "the job's environment reached the disk: {:?}",
        files_containing(&dir, "job-secret-value-7f3a")
    );

    let cleaned = execute("job-cleanup", &json!({"handle": handle}), &context);
    assert_eq!(cleaned.body["ok"], true, "{}", cleaned.body);
}

#[test]
fn a_cancelled_job_stops_and_reports_failed() {
    let dir = ralphus_test_support::TestDir::new("machine-job-cancel");
    let context = context(&dir);
    let command = if cfg!(windows) {
        "ping -n 60 127.0.0.1 >nul"
    } else {
        "sleep 60"
    };
    let started = execute("exec", &spec(&dir, "cell-cancel", command), &context);
    let handle = started.body["handle"]
        .as_str()
        .expect("a handle")
        .to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    while execute("status", &json!({"handle": handle}), &context).body["state"] != "running" {
        assert!(Instant::now() < deadline, "never started running");
        std::thread::sleep(Duration::from_millis(100));
    }
    let cancelled = execute("cancel", &json!({"handle": handle}), &context);
    assert_eq!(cancelled.body["ok"], true, "{}", cancelled.body);
    let finished = wait_terminal(&context, &handle);
    assert_eq!(finished["state"], "failed");
    assert_eq!(finished["result"]["summary"], "cancelled", "{finished}");
}
