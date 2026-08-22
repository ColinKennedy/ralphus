//! `ralphus-ssh-provider` -- a machine provider (RAL-185) that reaches any
//! host you already have SSH access to, addressed as `ssh:user@hostname`
//! (`ssh:hostname` falls back to the default user; a `~/.ssh/config` `Host`
//! alias works as-is) (RAL-200).
//!
//! **Scope is deliberately the `exec` verb only** (plus the cheap `ping`
//! reachability check the daemon already dispatches independently of
//! RAL-201). `provision`/`stream`/`status`/`cancel`/`cleanup` daemon-side
//! dispatch is a separate ticket (RAL-201); this provider runs a session by
//! syncing the local worktree onto the remote host and invoking
//! `ralphus-runner` there over a single non-interactive `ssh`, blocking until
//! it finishes -- see [`exec::run`].
//!
//! The wire contract (argv shape, stdin payload, one JSON object on stdout)
//! is modeled on `examples/providers/loopback.py`, the project's reference
//! provider, and documented in full in `docs/machine-providers.md`.
//!
//! stdout is reserved for that single JSON reply -- see [`protocol::reply`].
//! Everything else (diagnostics, forwarded `RALPHUS_EVENT:` lines) goes to
//! stderr, mirroring the daemon<->runner contract this whole project follows.
//!
//! Split into a library + thin binary (`src/main.rs`) solely so
//! `tests/exec_live_ssh.rs` -- the opt-in, live-remote-host integration test
//! -- can call [`exec::run`] in-process instead of shelling out to the built
//! executable.
#![allow(clippy::print_stdout)]

pub mod config;
pub mod exec;
pub mod ping;
pub mod protocol;
pub mod ssh;
pub mod transport;
pub mod uri;

/// Prefix a structured event line carries on stderr before its JSON body, so
/// the daemon's stderr-scraping (`daemon/src/runner.rs::EVENT_MARKER`) can
/// find it regardless of which layer (this provider, or the remote
/// `ralphus-runner` whose stderr we forward) actually emitted it.
pub const EVENT_MARKER: &str = "RALPHUS_EVENT: ";

/// Verbs this provider does not implement -- see the crate docs on scope.
/// Listed explicitly so an operator gets a pointed explanation instead of a
/// generic "unknown verb".
pub const UNIMPLEMENTED_VERBS: &[&str] = &[
    "provision",
    "run",
    "channel",
    "status",
    "stream",
    "cancel",
    "cleanup",
];
