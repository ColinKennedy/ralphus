//! The `ralphus initialize ...` commands. Read `AGENTS.md` in this folder
//! before adding or changing one: every guided exercise must run with no LLM,
//! locally and remotely, and be wired into CI.

pub mod exercise;
pub mod machine;
pub mod mailbox;
pub mod review;
pub mod server;
pub mod triage;
pub mod waypoint;

/// Every guided exercise (`ralphus initialize <name>`), i.e. every
/// `initialize` subcommand built on [`exercise::Exercise`]. This is the list
/// `scripts/check-initialize-exercises.sh` and CI must cover;
/// `cli/tests/initialize_exercise_ci_parity.rs` fails when they drift.
/// `server` (interactive setup) and `git` are not exercises.
pub const EXERCISES: &[&str] = &["machine", "mailbox", "review", "triage", "waypoint"];
