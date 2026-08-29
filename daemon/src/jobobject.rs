//! Confines the daemon's own process tree to a Windows Job Object so that
//! killing the daemon takes every subprocess it spawned down with it —
//! `ralphus-runner`, and anything the runner spawns in turn (harness/
//! claude-code sessions, verify subprocesses).
//!
//! Windows job membership is inherited by default: any process created by a
//! job member is itself added to the same job unless it explicitly requests
//! `CREATE_BREAKAWAY_FROM_JOB` (nothing in this codebase does). So the daemon
//! only has to join a job once, at startup — `ralphus-runner` and everything
//! it spawns join automatically as they're created, no code changes needed on
//! the Python side.
//!
//! This is unrelated to `CREATE_NEW_PROCESS_GROUP` (used elsewhere for
//! `CTRL_BREAK_EVENT` cancellation) — that flag only affects console signal
//! delivery, not job membership or process ancestry.
//!
//! There is no automated test that exercises the real self-assignment path:
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means dropping a `Job` handle that the
//! current process is assigned to terminates the current process, which would
//! take down the entire shared `cargo test` binary (it runs many tests in one
//! process). Verify this manually via `scripts/build-debug.sh`: start the
//! stack, run a task, then end the daemon from Task Manager and confirm the
//! runner (and any subprocess it spawned) disappear with it.

#[cfg(windows)]
mod imp {
    use win32job::{ExtendedLimitInfo, Job};

    /// Create a Job Object with kill-on-close semantics and assign the
    /// current process to it. The returned [`Job`] must be held for the
    /// daemon's entire lifetime — dropping it early closes the job's only
    /// handle, which (per `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) immediately
    /// terminates every process assigned to it, including the caller.
    ///
    /// Best-effort: job creation is a process-management enhancement, not a
    /// prerequisite for serving requests, so a failure here is logged and
    /// swallowed rather than stopping daemon startup.
    #[must_use = "dropping the returned guard immediately closes the job handle, which \
                  terminates the current process (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)"]
    pub fn confine_process_tree() -> Option<Job> {
        let job = match Job::create() {
            Ok(job) => job,
            Err(e) => {
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                crate::rlog!(WARNING, "ralphus [runner] could not create job object: {e}");
                return None;
            }
        };
        let mut info = ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        if let Err(e) = job.set_extended_limit_info(&info) {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [runner] could not set job object kill-on-close limit: {e}"
            );
            return None;
        }
        if let Err(e) = job.assign_current_process() {
            // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [runner] could not assign daemon process to job object: {e}"
            );
            return None;
        }
        // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [runner] daemon process tree confined to a job object (kill-on-close); \
             ralphus-runner and its descendants will join automatically"
        );
        Some(job)
    }
}

#[cfg(not(windows))]
mod imp {
    /// Job Objects are a Windows-only concept; nothing to do elsewhere.
    pub fn confine_process_tree() -> Option<()> {
        None
    }
}

pub use imp::confine_process_tree;

#[cfg(all(test, windows))]
mod tests {
    /// Exercises job creation and the exact kill-on-close limit
    /// `confine_process_tree` sets, stopping short of `assign_current_process`
    /// — see the module doc for why that call can't safely run inside the
    /// shared `cargo test` process.
    #[test]
    fn job_object_accepts_the_kill_on_close_limit() {
        let job = win32job::Job::create().expect("job object creation should succeed");
        let mut info = job
            .query_extended_limit_info()
            .expect("querying a fresh job's limit info should succeed");
        info.limit_kill_on_job_close();
        job.set_extended_limit_info(&info)
            .expect("setting the kill-on-close limit should succeed");
        // Dropping `job` here is safe: no process was ever assigned to it.
    }
}
