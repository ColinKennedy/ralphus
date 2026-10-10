//! The `ralphus initialize ...` commands. Read `AGENTS.md` in this folder
//! before adding or changing one: every guided exercise must run with no LLM,
//! locally and remotely, and be wired into CI.

pub mod exercise;
pub mod followup;
pub mod machine;
pub mod mailbox;
pub mod review;
pub mod solo_developer;
pub mod triage;
pub mod waypoint;

/// Every guided exercise (`ralphus initialize <name>`), i.e. every
/// `initialize` subcommand built on [`exercise::Exercise`]. This is the list
/// `scripts/check-initialize-exercises.sh` and CI must cover;
/// `cli/tests/initialize_exercise_ci_parity.rs` fails when they drift.
/// `solo_developer` (interactive setup) and `git` are not exercises, and neither is
/// `followup`: it is exempt from the exercise rules (see `AGENTS.md`) and
/// runs only locally, from its own entry in that script.
pub const EXERCISES: &[&str] = &["machine", "mailbox", "review", "triage", "waypoint"];
