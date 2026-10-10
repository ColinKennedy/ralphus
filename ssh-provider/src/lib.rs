//! `ralphus-ssh-provider` -- a machine provider (RAL-185) that reaches any
//! host you already have SSH access to, addressed as `ssh:user@hostname`
//! (`ssh:hostname` falls back to the default user; a `~/.ssh/config` `Host`
//! alias works as-is) (RAL-200).
//!
//! **`exec`, `status`, `stream`, `cancel`, `job-cleanup`, `ping`,
//! `provision`, `run`, `read-file`, `write-file`, `remove-path`, and
//! `cleanup`, and `channel` (one persistent `ssh` session serving many `run`s
//! -- see [`channel`]) are implemented.** Configured targets use durable asynchronous jobs
//! under their remote root -- see [`job`]. A legacy invocation without
//! target configuration keeps the synchronous path that syncs a local
//! worktree and blocks on `ralphus-runner` -- see [`exec::run`]. `provision`
//! (RAL-355
//! Phase 4) durably clones/fetches a project and creates a `git worktree`
//! per task branch under a target's configured `remote_root`, reused (never
//! re-cloned) across calls -- see [`provision::run`]. `run`/`read-file`/
//! `write-file`/`remove-path` (RAL-355 Phase 2/4 remainder) give a caller
//! structured, non-shell-string access to one VCS command or one file under
//! that same layout -- see [`fileops`]. `cleanup` (RAL-355 Phase 2
//! remainder) explicitly tears one workspace down -- see [`cleanup`].
//!
//! The wire contract (argv shape, stdin payload, one JSON object on stdout)
//! is modeled on `examples/providers/loopback.py`, the project's reference
//! provider, and documented in full in `docs/machine-providers.md`.
//!
//! stdout is reserved for that single JSON reply -- see [`protocol::reply`].
//! Everything else (diagnostics, forwarded `RALPHUS_EVENT:` lines) goes to
//! stderr, mirroring the daemon<->runner contract this whole project follows.
//!
//! Split into a library + thin binary (`src/main.rs`) so the opt-in live-host
//! integration tests can exercise each operation in-process.
#![allow(clippy::print_stdout)]

pub mod capabilities;
pub mod channel;
pub mod cleanup;
pub mod config;
pub mod exec;
pub mod fileops;
pub mod job;
pub mod layout;
pub mod materialize;
pub mod ping;
pub mod protocol;
pub mod provision;
pub mod runner_install;
pub mod ssh;
pub mod terminal;
pub mod transport;
pub mod uri;

/// Prefix a structured event line carries on stderr before its JSON body, so
/// the daemon's stderr-scraping (`daemon/src/runner.rs::EVENT_MARKER`) can
/// find it regardless of which layer (this provider, or the remote
/// `ralphus-runner` whose stderr we forward) actually emitted it.
pub const EVENT_MARKER: &str = "RALPHUS_EVENT: ";

/// Emits one structured event from this provider itself as an
/// [`EVENT_MARKER`] line on stderr (stdout carries the provider's single
/// JSON reply). The daemon's provider invocation forwards every such line
/// into Cartographer, filling squad/task/cell from the invocation it made.
/// Free text in `payload` is passed through
/// [`ralphus_core::redact::redact_secrets`] by callers that include remote
/// stderr; never put a clone URL, environment value, or spec body here.
pub fn emit_event(message: &str, level: &str, payload: serde_json::Value) {
    let body = serde_json::json!({
        "source": "ssh-provider",
        "message": message,
        "level": level,
        "payload": payload,
    });
    eprintln!("{EVENT_MARKER}{body}");
}

/// [`ralphus_core::redact::redact_secrets`] over an error string bound for
/// an [`emit_event`] payload -- provider errors can quote remote stderr.
#[must_use]
pub fn redacted(text: &str) -> String {
    ralphus_core::redact::redact_secrets(text).into_owned()
}

/// Verbs this provider does not implement -- see the crate docs on scope.
/// Listed explicitly so an operator gets a pointed explanation instead of a
/// generic "unknown verb". `retire` (automatic stale-worktree retirement) is
/// optional, and its "does not implement" reply is what the daemon reads as
/// this provider opting out -- a bare "unknown verb" would instead be recorded
/// as a failed retirement on every daily sweep.
pub const UNIMPLEMENTED_VERBS: &[&str] = &["retire"];
