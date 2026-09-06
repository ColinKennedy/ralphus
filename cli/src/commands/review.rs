//! `ralphus review <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `review` group (Guardian code review management) -- by far the widest
//! command tree in the CLI: ~24 top-level leaf commands plus five nested
//! subcommand groups (`upstream`, `pr`, `branch`, `checks`, `action`), each with
//! their own leaves.
//!
//! One pair of commands is deliberately not fully wired to the daemon:
//!
//! - [`ReviewBranchCommand::Terminal`] / [`ReviewChecksCommand::Terminal`]
//!   print the local resume command instead of calling the daemon's
//!   terminal-spawning endpoint, exactly like `cell.rs`'s
//!   `CellCommand::Terminal` -- the daemon's `open-terminal` endpoints
//!   spawn a GUI terminal on the *daemon's own host*, which is meaningless
//!   for a headless CLI.
//!
//! [`ReviewCommand::Squash`] and the `--clear KEY` handling in
//! [`dispatch_guardian_env`] were initially blocked on two `client.rs` gaps
//! (a missing `guardian_squash` wrapper, and `clear_vars` typed as `bool`
//! instead of the daemon's actual per-key list contract) -- both have since
//! been fixed directly in `client.rs`, so both commands are now fully wired.
//! The Python CLI's own `review squash` was never fixed the same way (it
//! calls a `client.guardian_squash(...)` method that was never defined on
//! the Python `DaemonClient`, so it always raised `AttributeError`) --
//! worth a note back to that side, not something this port needed to
//! reproduce.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::{DaemonClient, DaemonError, GuardianSettings};
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::{Scanner, UsageError};
use crate::selector::{
    DEFAULT_REVIEW_LIST_HINT, ResolvedGuardianSelector, SelectorError, guardian_view_uri,
    resolve_guardian_selector,
};

#[derive(Debug, Clone)]
pub enum ReviewCommand {
    Help,
    List {
        status: Option<String>,
        pr_ready: bool,
    },
    Show {
        selector: String,
    },
    Logs {
        selector: String,
    },
    Status {
        selector: String,
    },
    Worktrees {
        selector: String,
    },
    Create {
        name: String,
        base_branch: String,
        git_root: String,
        checks: Option<String>,
        skip_auto_build: bool,
        skip_worktrees: bool,
        review_type: Option<String>,
    },
    Rename {
        selector: String,
        name: String,
    },
    Cancel {
        selector: String,
    },
    Reopen {
        selector: String,
    },
    Delete {
        selector: String,
        yes: bool,
    },
    Settings {
        selector: String,
        skip_auto_build: Option<bool>,
        skip_worktrees: Option<bool>,
        resolver_agent: Option<String>,
        resolver_model: Option<String>,
        base_branch: Option<String>,
        auto_pr_feedback: Option<bool>,
        proof_scope: Option<String>,
        skip_auto_clean: Option<bool>,
        skip_base_updates: Option<bool>,
        /// RAL-307: this review's own override for whether a newly
        /// submitted PR's branch defaults to the worktree/feature branch
        /// name.
        match_pr_branch_name: Option<bool>,
    },
    BuildEnv(GuardianEnvArgs),
    ManualChecksEnv(GuardianEnvArgs),
    /// RAL-324: read-only listing of one review surface's resolved
    /// environment. `scope` is `worktree` (one contributing branch's review
    /// worktree), `build` (the finalize-time auto-build step), `tests` (the
    /// check gates, which run under the build step's layer), or
    /// `manual-checks`. Defaults to `worktree` when the selector names a
    /// branch, else `build`.
    Env {
        selector: String,
        scope: Option<String>,
    },
    Squash {
        selector: String,
        project: String,
        enabled: bool,
    },
    AddBranch {
        selector: String,
        branch: String,
    },
    Reorder {
        selector: String,
        order: String,
        disable: Option<String>,
        enable: Option<String>,
    },
    Merge {
        selector: String,
    },
    SyncPr {
        selector: String,
    },
    RestartMerge {
        selector: String,
    },
    StopMerge {
        selector: String,
    },
    ForceStart {
        selector: String,
    },
    Approve {
        selector: String,
    },
    Feedback {
        selector: String,
        text: String,
    },
    DismissReenable {
        selector: String,
    },
    MoveBranch {
        selector: String,
        to_review: String,
    },
    Upstream(ReviewUpstreamCommand),
    Pr(ReviewPrCommand),
    Branch(ReviewBranchCommand),
    Checks(ReviewChecksCommand),
    Action(ReviewActionCommand),
    UsageError(String),
}

/// Shared shape for `review build-env`/`review manual-checks-env` (RAL-203).
#[derive(Debug, Clone)]
pub struct GuardianEnvArgs {
    pub selector: String,
    pub set: Vec<String>,
    pub unset: Vec<String>,
    pub clear: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum ReviewUpstreamCommand {
    Help,
    List { selector: String },
    Set { selector: String, branch: String },
    UsageError(String),
}

#[derive(Debug, Clone)]
pub enum ReviewPrCommand {
    Help,
    Submit {
        selector: String,
        position: Option<i64>,
        combined: bool,
        alias: Option<String>,
        title: Option<String>,
        description: Option<String>,
        /// RAL-307: `Some(true)` when `--use-worktree-branch-name` is
        /// passed, overriding the review's own `match_pr_branch_name`
        /// setting for this submission only; `None` defers to it.
        use_worktree_branch_name: Option<bool>,
    },
    List {
        selector: String,
    },
    Show {
        pr_id: String,
    },
    Find {
        forge: String,
        repo: String,
        pr_number: i64,
    },
    Update {
        pr_id: String,
        pr_number: Option<i64>,
        pr_url: Option<String>,
        branch_alias: Option<String>,
        state: Option<String>,
    },
    Comments {
        pr_id: String,
    },
    PullFeedback {
        pr_id: String,
    },
    PullFromPr {
        pr_id: String,
    },
    /// RAL-317: guardian-wide, bulk-drop every currently open PR row and
    /// clear the registered forge PR stack number, so a later submission
    /// (auto or manual) starts a fresh stack instead of appending to one
    /// whose PRs were just unlinked.
    Unlink {
        selector: String,
    },
    UsageError(String),
}

#[derive(Debug, Clone)]
pub enum ReviewBranchCommand {
    Help,
    Enable { selector: String },
    Disable { selector: String },
    Terminal { selector: String, mode: String },
    UsageError(String),
}

#[derive(Debug, Clone)]
pub enum ReviewChecksCommand {
    Help,
    List {
        selector: String,
    },
    Run {
        selector: String,
        index: Vec<i64>,
        all: bool,
        input: Vec<(String, String)>,
    },
    Terminal {
        selector: String,
        mode: String,
    },
    UsageError(String),
}

#[derive(Debug, Clone)]
pub enum ReviewActionCommand {
    Help,
    List {
        selector: String,
    },
    Run {
        selector: String,
        index: i64,
        input: Vec<(String, String)>,
    },
    UsageError(String),
}

// ---- parsing ------------------------------------------------------------

#[must_use]
pub fn parse(args: &[String]) -> ReviewCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => ReviewCommand::Help,
        Some("list") => {
            let status = scanner.take_value("--status").ok().flatten();
            let pr_ready = scanner.take_bool("--pr-ready");
            ReviewCommand::List { status, pr_ready }
        }
        Some("show") => with_selector(scanner, |selector| ReviewCommand::Show { selector }),
        Some("logs") => with_selector(scanner, |selector| ReviewCommand::Logs { selector }),
        Some("status") => with_selector(scanner, |selector| ReviewCommand::Status { selector }),
        Some("worktrees") => {
            with_selector(scanner, |selector| ReviewCommand::Worktrees { selector })
        }
        Some("create") => {
            let checks = scanner.take_value("--checks").ok().flatten();
            let skip_auto_build = scanner.take_bool("--skip-auto-build");
            let skip_worktrees = scanner.take_bool("--skip-worktrees");
            let review_type = scanner.take_value("--review-type").ok().flatten();
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1), rest.get(2)) {
                (Some(name), Some(base_branch), Some(git_root)) => ReviewCommand::Create {
                    name: name.clone(),
                    base_branch: base_branch.clone(),
                    git_root: git_root.clone(),
                    checks,
                    skip_auto_build,
                    skip_worktrees,
                    review_type,
                },
                _ => ReviewCommand::UsageError(
                    "create requires <name> <base_branch> <git_root>".to_string(),
                ),
            }
        }
        Some("rename") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(name)) => ReviewCommand::Rename {
                    selector: selector.clone(),
                    name: name.clone(),
                },
                _ => ReviewCommand::UsageError("rename requires <selector> <name>".to_string()),
            }
        }
        Some("cancel") => with_selector(scanner, |selector| ReviewCommand::Cancel { selector }),
        Some("reopen") => with_selector(scanner, |selector| ReviewCommand::Reopen { selector }),
        Some("delete") => {
            let yes = scanner.take_bool("--yes");
            with_selector(scanner, |selector| ReviewCommand::Delete { selector, yes })
        }
        Some("settings") => {
            let skip_auto_build = take_tri_bool(&mut scanner, "--skip-auto-build");
            let skip_worktrees = take_tri_bool(&mut scanner, "--skip-worktrees");
            let resolver_agent = scanner.take_value("--resolver-agent").ok().flatten();
            let resolver_model = scanner.take_value("--resolver-model").ok().flatten();
            let base_branch = scanner.take_value("--base-branch").ok().flatten();
            let auto_pr_feedback = take_tri_bool(&mut scanner, "--auto-pr-feedback");
            let proof_scope = scanner.take_value("--proof-scope").ok().flatten();
            let skip_auto_clean = take_tri_bool(&mut scanner, "--skip-auto-clean");
            let skip_base_updates = take_tri_bool(&mut scanner, "--skip-base-updates");
            let match_pr_branch_name = take_tri_bool(&mut scanner, "--match-pr-branch-name");
            with_selector(scanner, |selector| ReviewCommand::Settings {
                selector,
                skip_auto_build,
                skip_worktrees,
                resolver_agent,
                resolver_model,
                base_branch,
                auto_pr_feedback,
                proof_scope,
                skip_auto_clean,
                skip_base_updates,
                match_pr_branch_name,
            })
        }
        Some("env") => {
            let scope = scanner.take_value("--scope").ok().flatten();
            match scope.as_deref() {
                None | Some("worktree" | "build" | "tests" | "manual-checks") => {
                    match scanner.remaining().into_iter().next() {
                        Some(selector) => ReviewCommand::Env { selector, scope },
                        None => ReviewCommand::UsageError(
                            "missing required <selector> argument".to_string(),
                        ),
                    }
                }
                Some(other) => ReviewCommand::UsageError(format!(
                    "unknown --scope {other:?} (expected one of: worktree, build, tests, manual-checks)"
                )),
            }
        }
        Some("build-env") => match parse_guardian_env(scanner) {
            Ok(a) => ReviewCommand::BuildEnv(a),
            Err(e) => ReviewCommand::UsageError(e.0),
        },
        Some("manual-checks-env") => match parse_guardian_env(scanner) {
            Ok(a) => ReviewCommand::ManualChecksEnv(a),
            Err(e) => ReviewCommand::UsageError(e.0),
        },
        Some("squash") => {
            let on = scanner.take_bool("--on");
            let off = scanner.take_bool("--off");
            let enabled = match (on, off) {
                (true, false) => Some(true),
                (false, true) => Some(false),
                _ => None,
            };
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1), enabled) {
                (Some(selector), Some(project), Some(enabled)) => ReviewCommand::Squash {
                    selector: selector.clone(),
                    project: project.clone(),
                    enabled,
                },
                (_, _, None) => ReviewCommand::UsageError(
                    "squash requires exactly one of --on/--off".to_string(),
                ),
                _ => ReviewCommand::UsageError("squash requires <selector> <project>".to_string()),
            }
        }
        Some("add-branch") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(branch)) => ReviewCommand::AddBranch {
                    selector: selector.clone(),
                    branch: branch.clone(),
                },
                _ => {
                    ReviewCommand::UsageError("add-branch requires <selector> <branch>".to_string())
                }
            }
        }
        Some("reorder") => {
            let disable = scanner.take_value("--disable").ok().flatten();
            let enable = scanner.take_value("--enable").ok().flatten();
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(order)) => ReviewCommand::Reorder {
                    selector: selector.clone(),
                    order: order.clone(),
                    disable,
                    enable,
                },
                _ => ReviewCommand::UsageError("reorder requires <selector> <order>".to_string()),
            }
        }
        Some("merge") => with_selector(scanner, |selector| ReviewCommand::Merge { selector }),
        Some("sync-pr") => with_selector(scanner, |selector| ReviewCommand::SyncPr { selector }),
        Some("restart-merge") => {
            with_selector(scanner, |selector| ReviewCommand::RestartMerge { selector })
        }
        Some("stop-merge") => {
            with_selector(scanner, |selector| ReviewCommand::StopMerge { selector })
        }
        Some("force-start") => {
            with_selector(scanner, |selector| ReviewCommand::ForceStart { selector })
        }
        Some("approve") => with_selector(scanner, |selector| ReviewCommand::Approve { selector }),
        Some("feedback") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(text)) => ReviewCommand::Feedback {
                    selector: selector.clone(),
                    text: text.clone(),
                },
                _ => ReviewCommand::UsageError("feedback requires <selector> <text>".to_string()),
            }
        }
        Some("dismiss-reenable") => with_selector(scanner, |selector| {
            ReviewCommand::DismissReenable { selector }
        }),
        Some("move-branch") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(to_review)) => ReviewCommand::MoveBranch {
                    selector: selector.clone(),
                    to_review: to_review.clone(),
                },
                _ => ReviewCommand::UsageError(
                    "move-branch requires <selector> <to_review>".to_string(),
                ),
            }
        }
        Some("upstream") => ReviewCommand::Upstream(parse_upstream(&scanner.remaining())),
        Some("pr") => ReviewCommand::Pr(parse_pr(&scanner.remaining())),
        Some("branch") => ReviewCommand::Branch(parse_branch(&scanner.remaining())),
        Some("checks") => ReviewCommand::Checks(parse_checks(&scanner.remaining())),
        Some("action") => ReviewCommand::Action(parse_action(&scanner.remaining())),
        Some(other) => ReviewCommand::UsageError(format!("unknown review subcommand: {other}")),
    }
}

fn with_selector(scanner: Scanner, make: impl FnOnce(String) -> ReviewCommand) -> ReviewCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => ReviewCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

/// `--flag`/`--no-flag` tri-state, mirroring Python's
/// `argparse.BooleanOptionalAction` (default `None`). `name` must be the
/// positive spelling (e.g. `"--skip-auto-build"`); the negative form is
/// derived by inserting `no-` after the leading `--`.
pub(crate) fn take_tri_bool(scanner: &mut Scanner, name: &str) -> Option<bool> {
    let neg = format!("--no-{}", &name[2..]);
    let mut result = None;
    if scanner.take_bool(&neg) {
        result = Some(false);
    }
    if scanner.take_bool(name) {
        result = Some(true);
    }
    result
}

fn parse_guardian_env(mut scanner: Scanner) -> Result<GuardianEnvArgs, UsageError> {
    let set = scanner.take_repeated("--set")?;
    let unset = scanner.take_repeated("--unset")?;
    let clear = scanner.take_repeated("--clear")?;
    let selector = scanner
        .remaining()
        .into_iter()
        .next()
        .ok_or_else(|| UsageError("missing required <selector> argument".to_string()))?;
    Ok(GuardianEnvArgs {
        selector,
        set,
        unset,
        clear,
    })
}

fn parse_kv(raw: &str) -> Result<(String, String), UsageError> {
    match raw.split_once('=') {
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => Err(UsageError(format!(
            "--input: expected NAME=VALUE, got '{raw}'"
        ))),
    }
}

fn parse_kv_list(raw: &[String]) -> Result<Vec<(String, String)>, UsageError> {
    raw.iter().map(|s| parse_kv(s)).collect()
}

fn parse_upstream(args: &[String]) -> ReviewUpstreamCommand {
    let scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => ReviewUpstreamCommand::Help,
        Some("list") => {
            with_selector_upstream(scanner, |selector| ReviewUpstreamCommand::List { selector })
        }
        Some("set") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1)) {
                (Some(selector), Some(branch)) => ReviewUpstreamCommand::Set {
                    selector: selector.clone(),
                    branch: branch.clone(),
                },
                _ => ReviewUpstreamCommand::UsageError(
                    "set requires <selector> <branch>".to_string(),
                ),
            }
        }
        Some(other) => ReviewUpstreamCommand::UsageError(format!(
            "unknown review upstream subcommand: {other}"
        )),
    }
}

fn with_selector_upstream(
    scanner: Scanner,
    make: impl FnOnce(String) -> ReviewUpstreamCommand,
) -> ReviewUpstreamCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => {
            ReviewUpstreamCommand::UsageError("missing required <selector> argument".to_string())
        }
    }
}

fn parse_pr(args: &[String]) -> ReviewPrCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => ReviewPrCommand::Help,
        Some("submit") => {
            let position = match scanner.take_parsed::<i64>("--position") {
                Ok(v) => v,
                Err(e) => return ReviewPrCommand::UsageError(e.0),
            };
            let combined = scanner.take_bool("--combined");
            let alias = scanner.take_value("--alias").ok().flatten();
            let title = scanner.take_value("--title").ok().flatten();
            let description = scanner.take_value("--description").ok().flatten();
            let use_worktree_branch_name = scanner
                .take_bool("--use-worktree-branch-name")
                .then_some(true);
            if position.is_some() == combined {
                return ReviewPrCommand::UsageError(
                    "submit requires exactly one of --position or --combined".to_string(),
                );
            }
            match scanner.remaining().into_iter().next() {
                Some(selector) => ReviewPrCommand::Submit {
                    selector,
                    position,
                    combined,
                    alias,
                    title,
                    description,
                    use_worktree_branch_name,
                },
                None => {
                    ReviewPrCommand::UsageError("missing required <selector> argument".to_string())
                }
            }
        }
        Some("list") => with_selector_pr(scanner, |selector| ReviewPrCommand::List { selector }),
        Some("show") => match scanner.remaining().into_iter().next() {
            Some(pr_id) => ReviewPrCommand::Show { pr_id },
            None => ReviewPrCommand::UsageError("missing required <pr_id> argument".to_string()),
        },
        Some("find") => {
            let rest = scanner.remaining();
            match (rest.first(), rest.get(1), rest.get(2)) {
                (Some(forge), Some(repo), Some(pr_number)) => match pr_number.parse::<i64>() {
                    Ok(n) => ReviewPrCommand::Find {
                        forge: forge.clone(),
                        repo: repo.clone(),
                        pr_number: n,
                    },
                    Err(_) => ReviewPrCommand::UsageError(format!(
                        "pr_number: invalid value '{pr_number}'"
                    )),
                },
                _ => ReviewPrCommand::UsageError(
                    "find requires <forge> <repo> <pr_number>".to_string(),
                ),
            }
        }
        Some("update") => {
            let pr_number = match scanner.take_parsed::<i64>("--pr-number") {
                Ok(v) => v,
                Err(e) => return ReviewPrCommand::UsageError(e.0),
            };
            let pr_url = scanner.take_value("--pr-url").ok().flatten();
            let branch_alias = scanner.take_value("--branch-alias").ok().flatten();
            let state = scanner.take_value("--state").ok().flatten();
            match scanner.remaining().into_iter().next() {
                Some(pr_id) => ReviewPrCommand::Update {
                    pr_id,
                    pr_number,
                    pr_url,
                    branch_alias,
                    state,
                },
                None => {
                    ReviewPrCommand::UsageError("missing required <pr_id> argument".to_string())
                }
            }
        }
        Some("comments") => match scanner.remaining().into_iter().next() {
            Some(pr_id) => ReviewPrCommand::Comments { pr_id },
            None => ReviewPrCommand::UsageError("missing required <pr_id> argument".to_string()),
        },
        Some("pull-feedback") => match scanner.remaining().into_iter().next() {
            Some(pr_id) => ReviewPrCommand::PullFeedback { pr_id },
            None => ReviewPrCommand::UsageError("missing required <pr_id> argument".to_string()),
        },
        Some("pull-from-pr") => match scanner.remaining().into_iter().next() {
            Some(pr_id) => ReviewPrCommand::PullFromPr { pr_id },
            None => ReviewPrCommand::UsageError("missing required <pr_id> argument".to_string()),
        },
        Some("unlink") => {
            with_selector_pr(scanner, |selector| ReviewPrCommand::Unlink { selector })
        }
        Some(other) => {
            ReviewPrCommand::UsageError(format!("unknown review pr subcommand: {other}"))
        }
    }
}

fn with_selector_pr(
    scanner: Scanner,
    make: impl FnOnce(String) -> ReviewPrCommand,
) -> ReviewPrCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => ReviewPrCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

fn parse_branch(args: &[String]) -> ReviewBranchCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => ReviewBranchCommand::Help,
        Some("enable") => {
            with_selector_branch(scanner, |selector| ReviewBranchCommand::Enable { selector })
        }
        Some("disable") => with_selector_branch(scanner, |selector| ReviewBranchCommand::Disable {
            selector,
        }),
        Some("terminal") => {
            let mode = match parse_terminal_mode(&mut scanner) {
                Ok(m) => m,
                Err(e) => return ReviewBranchCommand::UsageError(e),
            };
            with_selector_branch(scanner, |selector| ReviewBranchCommand::Terminal {
                selector,
                mode,
            })
        }
        Some(other) => {
            ReviewBranchCommand::UsageError(format!("unknown review branch subcommand: {other}"))
        }
    }
}

fn with_selector_branch(
    scanner: Scanner,
    make: impl FnOnce(String) -> ReviewBranchCommand,
) -> ReviewBranchCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => ReviewBranchCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

/// Shared `--mode open|readonly` (default `open`) parsing for `review branch
/// terminal`/`review checks terminal`, mirroring `cell.rs`'s
/// `CellCommand::Terminal` flag.
fn parse_terminal_mode(scanner: &mut Scanner) -> Result<String, String> {
    match scanner.take_value("--mode").ok().flatten() {
        Some(m) if m == "open" || m == "readonly" => Ok(m),
        Some(other) => Err(format!(
            "--mode: invalid choice '{other}' (choose from 'open', 'readonly')"
        )),
        None => Ok("open".to_string()),
    }
}

fn parse_checks(args: &[String]) -> ReviewChecksCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => ReviewChecksCommand::Help,
        Some("list") => {
            with_selector_checks(scanner, |selector| ReviewChecksCommand::List { selector })
        }
        Some("run") => {
            let raw_index = match scanner.take_repeated("--index") {
                Ok(v) => v,
                Err(e) => return ReviewChecksCommand::UsageError(e.0),
            };
            let mut index = Vec::new();
            for raw in raw_index {
                match raw.parse::<i64>() {
                    Ok(n) => index.push(n),
                    Err(_) => {
                        return ReviewChecksCommand::UsageError(format!(
                            "--index: invalid value '{raw}'"
                        ));
                    }
                }
            }
            let all = scanner.take_bool("--all");
            let raw_inputs = match scanner.take_repeated("--input") {
                Ok(v) => v,
                Err(e) => return ReviewChecksCommand::UsageError(e.0),
            };
            let input = match parse_kv_list(&raw_inputs) {
                Ok(v) => v,
                Err(e) => return ReviewChecksCommand::UsageError(e.0),
            };
            with_selector_checks(scanner, |selector| ReviewChecksCommand::Run {
                selector,
                index,
                all,
                input,
            })
        }
        Some("terminal") => {
            let mode = match parse_terminal_mode(&mut scanner) {
                Ok(m) => m,
                Err(e) => return ReviewChecksCommand::UsageError(e),
            };
            with_selector_checks(scanner, |selector| ReviewChecksCommand::Terminal {
                selector,
                mode,
            })
        }
        Some(other) => {
            ReviewChecksCommand::UsageError(format!("unknown review checks subcommand: {other}"))
        }
    }
}

fn with_selector_checks(
    scanner: Scanner,
    make: impl FnOnce(String) -> ReviewChecksCommand,
) -> ReviewChecksCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => ReviewChecksCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

fn parse_action(args: &[String]) -> ReviewActionCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => ReviewActionCommand::Help,
        Some("list") => {
            with_selector_action(scanner, |selector| ReviewActionCommand::List { selector })
        }
        Some("run") => {
            let index = match scanner.take_parsed::<i64>("--index") {
                Ok(Some(v)) => v,
                Ok(None) => {
                    return ReviewActionCommand::UsageError("run requires --index <n>".to_string());
                }
                Err(e) => return ReviewActionCommand::UsageError(e.0),
            };
            let raw_inputs = match scanner.take_repeated("--input") {
                Ok(v) => v,
                Err(e) => return ReviewActionCommand::UsageError(e.0),
            };
            let input = match parse_kv_list(&raw_inputs) {
                Ok(v) => v,
                Err(e) => return ReviewActionCommand::UsageError(e.0),
            };
            with_selector_action(scanner, |selector| ReviewActionCommand::Run {
                selector,
                index,
                input,
            })
        }
        Some(other) => {
            ReviewActionCommand::UsageError(format!("unknown review action subcommand: {other}"))
        }
    }
}

fn with_selector_action(
    scanner: Scanner,
    make: impl FnOnce(String) -> ReviewActionCommand,
) -> ReviewActionCommand {
    match scanner.remaining().into_iter().next() {
        Some(selector) => make(selector),
        None => ReviewActionCommand::UsageError("missing required <selector> argument".to_string()),
    }
}

// ---- shared dispatch helpers ---------------------------------------------

/// Whether `guardian` (a `GuardianView` from `GET /api/guardians`) is a
/// plausible PR-submission candidate -- `--pr-ready`'s filter predicate.
pub fn is_pr_ready(guardian: &Value) -> bool {
    const PR_READY_STATUSES: [&str; 2] = ["in_review", "approved"];
    let status = guardian["status"].as_str().unwrap_or_default();
    if !PR_READY_STATUSES.contains(&status) {
        return false;
    }
    let mp = &guardian["merge_progress"];
    let total = mp["total"].as_i64().unwrap_or(0);
    let done = mp["done"].as_i64().unwrap_or(0);
    let failed = mp["failed"].as_i64().unwrap_or(0);
    total > 0 && failed == 0 && done == total
}

fn review_status_verdict(g: &Value) -> String {
    let status = g["status"].as_str().unwrap_or_default();
    if g["ready"].as_bool().unwrap_or(false) {
        return "ready for review".to_string();
    }
    if status == "collecting" {
        let mp = &g["merge_progress"];
        return format!(
            "collecting ({}/{} branches merged)",
            mp["done"].as_i64().unwrap_or(0),
            mp["total"].as_i64().unwrap_or(0)
        );
    }
    if status == "merging" {
        return "merging".to_string();
    }
    status.to_string()
}

fn with_uri(mut payload: Value, uri: String) -> Value {
    if let Value::Object(map) = &mut payload {
        let mut ordered = serde_json::Map::new();
        ordered.insert("uri".to_string(), Value::String(uri));
        ordered.extend(map.clone());
        *map = ordered;
    }
    payload
}

fn confirm(prompt: &str) -> bool {
    use std::io::Write as _;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

fn fail_selector(opts: &GlobalOpts, e: SelectorError) -> i32 {
    let err = CommandError::Selector(e);
    err.print(opts.json, None);
    err.exit_code()
}

fn fail_daemon(opts: &GlobalOpts, e: DaemonError) -> i32 {
    let err = CommandError::Daemon(e);
    err.print(opts.json, None);
    err.exit_code()
}

/// Prints a `SelectorError`-shaped message (matching `_print_selector_error`)
/// and returns `code` -- used for the handful of Python handlers that print
/// via `_print_selector_error` but then return a hardcoded exit code other
/// than the usual `2` (e.g. "no manual checks available" -> `1`).
fn fail_message(opts: &GlobalOpts, message: String, code: i32) -> i32 {
    CommandError::Selector(SelectorError(message)).print(opts.json, None);
    code
}

/// Mirrors `_resolve_guardian_branch_or_none`: resolves `selector` and
/// requires it to name a branch (`guardian#branch` / `?worktree=`), printing
/// and returning the right exit code itself on failure so call sites can
/// just propagate the `Err(code)`.
fn resolve_branch(
    opts: &GlobalOpts,
    client: &DaemonClient,
    selector: &str,
) -> Result<ResolvedGuardianSelector, i32> {
    match resolve_guardian_selector(client, selector, DEFAULT_REVIEW_LIST_HINT) {
        Ok(r) if r.branch_id.is_some() => Ok(r),
        Ok(_) => Err(fail_message(
            opts,
            format!("'{selector}' does not name a branch (use guardian#branch)"),
            2,
        )),
        Err(e) => Err(fail_selector(opts, e)),
    }
}

/// Parses repeated `--set KEY=VALUE`-style flags into a map -- a later
/// duplicate key overwrites an earlier one. Ported from Python's
/// `_parse_environment_flags`.
pub fn parse_environment_flags(
    raw: &[String],
    flag_name: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut result = BTreeMap::new();
    for item in raw {
        match item.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => {
                result.insert(k.trim().to_string(), v.to_string());
            }
            _ => return Err(format!("{flag_name} expects KEY=VALUE, got '{item}'")),
        }
    }
    Ok(result)
}

/// Appended to a resumed agent's context in `--mode readonly` (mirrors
/// Python's `_READONLY_RESUME_INSTRUCTIONS`, duplicated here the same way
/// `cell.rs` duplicates it -- see that module's `agent_resume_command`
/// doc comment for why there is no single shared implementation).
const READONLY_RESUME_INSTRUCTIONS: &str = "You are in read-only mode. You may only read files. Do NOT write, \
edit, delete, commit, or push anything.";

/// Builds the local CLI command that resumes `agent_session_id`; see
/// `cell.rs::agent_resume_command` (identical logic, kept as its own copy
/// per-module -- Python's own `_agent_resume_command` is likewise a
/// hand-mirrored duplicate with no shared boundary to call into).
pub fn agent_resume_command(
    agent: Option<&str>,
    agent_session_id: &str,
    mode: &str,
) -> Vec<String> {
    let mut cmd: Vec<String>;
    if matches!(agent, Some("codex") | Some("codex-cli")) {
        // The top-level interactive `codex resume`, not `codex exec resume`
        // (Codex's non-interactive headless mode, which requires a prompt
        // argument or piped stdin and fails immediately with "No prompt
        // provided" otherwise -- exactly the reported symptom of resuming
        // this way into an interactive terminal with nothing to pipe in).
        cmd = vec!["codex".to_string()];
        if mode == "readonly" {
            cmd.push("-c".to_string());
            cmd.push(format!(
                "developer_instructions={READONLY_RESUME_INSTRUCTIONS}"
            ));
        }
        cmd.push("resume".to_string());
        cmd.push(agent_session_id.to_string());
    } else if matches!(agent, Some("pi")) {
        cmd = vec![
            "pi".to_string(),
            "--session".to_string(),
            agent_session_id.to_string(),
            "--approve".to_string(),
        ];
        if mode == "readonly" {
            cmd.push("--append-system-prompt".to_string());
            cmd.push(READONLY_RESUME_INSTRUCTIONS.to_string());
        }
    } else {
        cmd = vec![
            "claude".to_string(),
            "--resume".to_string(),
            agent_session_id.to_string(),
        ];
        if mode == "readonly" {
            cmd.push("--dangerously-skip-permissions".to_string());
            cmd.push("--append-system-prompt".to_string());
            cmd.push(READONLY_RESUME_INSTRUCTIONS.to_string());
        }
    }
    cmd
}

/// Prints a resolved check/action command's `cwd`/`command`, and (if
/// `env_keys` is given) which environment-variable names it runs under --
/// never the values (RAL-203). Ported from Python's
/// `_print_command_with_cwd`.
fn print_command_with_cwd(cwd: &str, command: &str, env_keys: Option<&[String]>) {
    let mut kv: Vec<(&str, String)> =
        vec![("cwd", cwd.to_string()), ("command", command.to_string())];
    if let Some(keys) = env_keys {
        if !keys.is_empty() {
            let joined = keys.join(", ");
            kv.push((
                "env",
                format!("{} var(s) inherited (values hidden): {joined}", keys.len()),
            ));
        }
    }
    crate::output::print_kv(&kv);
}

/// Substitutes `{name}` placeholders declared in `check["inputs"]` into
/// `command` (RAL-164), preferring `overrides` (CLI `--input` flags, last
/// occurrence wins), then the guardian's stored `input_values`, then the
/// input's own literal default. Returns `(resolved, missing)`. Ported from
/// Python's `_resolve_check_inputs`.
pub fn resolve_check_inputs(
    command: &str,
    check: &Value,
    input_values: &Value,
    overrides: &[(String, String)],
) -> (String, Vec<String>) {
    let mut resolved = command.to_string();
    let mut missing = Vec::new();
    for inp in check["inputs"].as_array().into_iter().flatten() {
        let name = inp["name"].as_str().unwrap_or_default();
        let default = inp["default"].as_str().unwrap_or_default();
        let value = overrides
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| input_values[name].as_str().map(str::to_string))
            .or_else(|| {
                if default.is_empty() {
                    None
                } else {
                    Some(default.to_string())
                }
            });
        match value {
            Some(v) => resolved = resolved.replace(&format!("{{{name}}}"), &v),
            None => missing.push(name.to_string()),
        }
    }
    (resolved, missing)
}

fn value_or_dash(v: &Value) -> String {
    if v.is_null() {
        "-".to_string()
    } else {
        v.to_string()
    }
}

fn print_guardian_env_summary(g: &Value) {
    let build = g["build_env_overrides"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let manual = g["manual_checks_env_overrides"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if build.is_empty() && manual.is_empty() {
        return;
    }
    println!("\nenv overrides:");
    for (label, overrides) in [("build", &build), ("manual-checks", &manual)] {
        if overrides.is_empty() {
            continue;
        }
        println!("  {label}:");
        for (key, value) in overrides.iter() {
            let status = if value.is_null() {
                "unset"
            } else {
                "override (value hidden)"
            };
            println!("    {key}: {status}");
        }
    }
}

// ---- dispatch -------------------------------------------------------------

#[must_use]
pub fn dispatch(cmd: ReviewCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        ReviewCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["review"]).expect("review help exists")
            );
            0
        }
        ReviewCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ReviewCommand::List { status, pr_ready } => run_and_report(opts, None, || {
            let guardians = client.guardian_list()?;
            let mut list = guardians.as_array().cloned().unwrap_or_default();
            if let Some(status) = &status {
                let wanted: Vec<String> = status
                    .split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                list.retain(|g| {
                    wanted.contains(&g["status"].as_str().unwrap_or_default().to_lowercase())
                });
            }
            if pr_ready {
                list.retain(is_pr_ready);
            }
            let result = Value::Array(list);
            emit(opts, &result, render_review_list);
            Ok(())
        }),
        ReviewCommand::Show { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let guardian = client.guardian_get(&resolved.guardian_id)?;
            let uri = guardian_view_uri(&guardian, Some(&resolved));
            let payload = with_uri(guardian, uri);
            emit(opts, &payload, render_review_detail);
            Ok(())
        }),
        ReviewCommand::Logs { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let events = client.guardian_logs(&resolved.guardian_id)?;
            emit(opts, &events, render_events);
            Ok(())
        }),
        ReviewCommand::Status { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let guardian = client.guardian_get(&resolved.guardian_id)?;
            emit(opts, &guardian, render_review_status);
            Ok(())
        }),
        ReviewCommand::Worktrees { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let guardian = client.guardian_get(&resolved.guardian_id)?;
            emit(opts, &guardian, render_review_worktrees);
            Ok(())
        }),
        ReviewCommand::Create {
            name,
            base_branch,
            git_root,
            checks,
            skip_auto_build,
            skip_worktrees,
            review_type,
        } => run_and_report(opts, None, || {
            let checks_vec: Vec<String> = checks
                .as_deref()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let result = client.guardian_create(
                &name,
                &base_branch,
                &git_root,
                Some(&checks_vec),
                skip_auto_build,
                skip_worktrees,
                review_type.as_deref(),
            )?;
            emit(opts, &result, |r| {
                println!("{}", r["id"].as_str().unwrap_or_default())
            });
            Ok(())
        }),
        ReviewCommand::Rename { selector, name } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_rename(&resolved.guardian_id, &name)?;
            emit(opts, &result, |_| {
                println!("{selector} renamed to '{name}'")
            });
            Ok(())
        }),
        ReviewCommand::Cancel { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_cancel(&resolved.guardian_id)?;
            emit(opts, &result, |r| println!("{selector} -> {}", r["state"]));
            Ok(())
        }),
        ReviewCommand::Reopen { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_reopen(&resolved.guardian_id)?;
            emit(opts, &result, |_| {
                println!("{selector} reopened; staging ready branches")
            });
            Ok(())
        }),
        ReviewCommand::Delete { selector, yes } => {
            if !yes && !confirm(&format!("Delete {selector}? This cannot be undone. [y/N] ")) {
                println!("aborted");
                return 1;
            }
            run_and_report(opts, None, || {
                let resolved =
                    resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
                let result = client.guardian_delete(&resolved.guardian_id)?;
                emit(opts, &result, |r| println!("{selector} -> {}", r["state"]));
                Ok(())
            })
        }
        ReviewCommand::Settings {
            selector,
            skip_auto_build,
            skip_worktrees,
            resolver_agent,
            resolver_model,
            base_branch,
            auto_pr_feedback,
            proof_scope,
            skip_auto_clean,
            skip_base_updates,
            match_pr_branch_name,
        } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let settings = GuardianSettings {
                skip_auto_build,
                skip_worktrees,
                resolver_agent: resolver_agent.as_deref(),
                resolver_model: resolver_model.as_deref(),
                base_branch: base_branch.as_deref(),
                auto_pr_feedback,
                proof_scope: proof_scope.as_deref(),
                proof_skip_auto_clean: skip_auto_clean,
                skip_base_updates,
                match_pr_branch_name,
            };
            let result = client.guardian_settings(&resolved.guardian_id, &settings)?;
            emit(opts, &result, |_| println!("{selector} settings updated"));
            Ok(())
        }),
        ReviewCommand::Env { selector, scope } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let scope =
                scope.unwrap_or_else(|| crate::commands::env::default_review_scope(&resolved));
            let path = crate::commands::env::review_path(&resolved, &scope, &selector)?;
            let view = client.env_view(&path)?;
            emit(opts, &view, crate::commands::env::render);
            Ok(())
        }),
        ReviewCommand::BuildEnv(args) => {
            dispatch_guardian_env(opts, &client, args, GuardianEnvSection::Build)
        }
        ReviewCommand::ManualChecksEnv(args) => {
            dispatch_guardian_env(opts, &client, args, GuardianEnvSection::ManualChecks)
        }
        ReviewCommand::Squash {
            selector,
            project,
            enabled,
        } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_squash(&resolved.guardian_id, &project, enabled)?;
            emit(opts, &result, |_| {
                println!(
                    "{project}: squash {}",
                    if enabled { "enabled" } else { "disabled" }
                )
            });
            Ok(())
        }),
        ReviewCommand::AddBranch { selector, branch } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_add_branch(&resolved.guardian_id, &branch)?;
            emit(opts, &result, |r| {
                println!("added '{branch}' at position {}", r["position"])
            });
            Ok(())
        }),
        ReviewCommand::Reorder {
            selector,
            order,
            disable,
            enable,
        } => run_and_report(opts, None, || {
            let order_vec: Vec<String> = order
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let mut enabled_map = serde_json::Map::new();
            for b in disable.as_deref().unwrap_or("").split(',') {
                let b = b.trim();
                if !b.is_empty() {
                    enabled_map.insert(b.to_string(), Value::Bool(false));
                }
            }
            for b in enable.as_deref().unwrap_or("").split(',') {
                let b = b.trim();
                if !b.is_empty() {
                    enabled_map.insert(b.to_string(), Value::Bool(true));
                }
            }
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_arrange(
                &resolved.guardian_id,
                &order_vec,
                Some(&Value::Object(enabled_map)),
            )?;
            emit(opts, &result, |_| {
                println!("{selector} reordered; rebase started")
            });
            Ok(())
        }),
        ReviewCommand::Merge { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_merge(&resolved.guardian_id)?;
            emit(opts, &result, |_| println!("{selector} merge started"));
            Ok(())
        }),
        ReviewCommand::SyncPr { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_sync_pr(&resolved.guardian_id)?;
            emit(opts, &result, |_| {
                println!("{selector} checking the forge for a stack reorder")
            });
            Ok(())
        }),
        ReviewCommand::RestartMerge { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_cancel_and_merge(&resolved.guardian_id)?;
            emit(opts, &result, |_| println!("{selector} merge restarted"));
            Ok(())
        }),
        ReviewCommand::StopMerge { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_stop(&resolved.guardian_id)?;
            emit(opts, &result, |_| println!("{selector} merge stopped"));
            Ok(())
        }),
        ReviewCommand::ForceStart { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_force_start(&resolved.guardian_id)?;
            emit(opts, &result, |_| println!("{selector} force-started"));
            Ok(())
        }),
        ReviewCommand::Approve { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(&client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_approve(&resolved.guardian_id)?;
            emit(opts, &result, |r| println!("{selector} -> {}", r["state"]));
            Ok(())
        }),
        ReviewCommand::Feedback { selector, text } => {
            let resolved = match resolve_branch(opts, &client, &selector) {
                Ok(r) => r,
                Err(code) => return code,
            };
            match client.guardian_feedback(
                &resolved.guardian_id,
                resolved.branch_id.as_deref().unwrap_or_default(),
                &text,
            ) {
                Ok(result) => {
                    emit(opts, &result, |_| println!("feedback posted on {selector}"));
                    0
                }
                Err(e) => fail_daemon(opts, e),
            }
        }
        ReviewCommand::DismissReenable { selector } => {
            let resolved = match resolve_branch(opts, &client, &selector) {
                Ok(r) => r,
                Err(code) => return code,
            };
            match client.guardian_dismiss_reenable(
                &resolved.guardian_id,
                resolved.branch_id.as_deref().unwrap_or_default(),
            ) {
                Ok(result) => {
                    emit(opts, &result, |_| {
                        println!("{selector} re-enable notice dismissed")
                    });
                    0
                }
                Err(e) => fail_daemon(opts, e),
            }
        }
        ReviewCommand::MoveBranch {
            selector,
            to_review,
        } => {
            let resolved = match resolve_branch(opts, &client, &selector) {
                Ok(r) => r,
                Err(code) => return code,
            };
            let to_resolved =
                match resolve_guardian_selector(&client, &to_review, DEFAULT_REVIEW_LIST_HINT) {
                    Ok(r) => r,
                    Err(e) => return fail_selector(opts, e),
                };
            match client.guardian_move_branch(
                &resolved.guardian_id,
                resolved.branch_id.as_deref().unwrap_or_default(),
                &to_resolved.guardian_id,
            ) {
                Ok(result) => {
                    emit(opts, &result, |_| {
                        println!("{selector} moved to {to_review}; rebuilding both reviews")
                    });
                    0
                }
                Err(e) => fail_daemon(opts, e),
            }
        }
        ReviewCommand::Upstream(c) => dispatch_upstream(c, opts, &client),
        ReviewCommand::Pr(c) => dispatch_pr(c, opts, &client),
        ReviewCommand::Branch(c) => dispatch_branch(c, opts, &client),
        ReviewCommand::Checks(c) => dispatch_checks(c, opts, &client),
        ReviewCommand::Action(c) => dispatch_action(c, opts, &client),
    }
}

/// Which guardian-level env-override layer [`dispatch_guardian_env`]
/// targets, mirroring `daemon/src/server.rs::GuardianEnvSection`.
#[derive(Clone, Copy)]
enum GuardianEnvSection {
    Build,
    ManualChecks,
}

impl GuardianEnvSection {
    fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::ManualChecks => "manual-checks",
        }
    }
}

fn dispatch_guardian_env(
    opts: &GlobalOpts,
    client: &DaemonClient,
    args: GuardianEnvArgs,
    section: GuardianEnvSection,
) -> i32 {
    let GuardianEnvArgs {
        selector,
        set,
        unset,
        clear,
    } = args;
    let set_map = match parse_environment_flags(&set, "--set") {
        Ok(m) => m,
        Err(msg) => return fail_message(opts, msg, 2),
    };
    if set_map.is_empty() && unset.is_empty() && clear.is_empty() {
        return fail_message(
            opts,
            "at least one of --set/--unset/--clear is required".to_string(),
            2,
        );
    }
    run_and_report(opts, None, || {
        let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
        let set_value = Value::Object(
            set_map
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        );
        let clear_opt = (!clear.is_empty()).then_some(clear.as_slice());
        match section {
            GuardianEnvSection::Build => client.set_guardian_build_env(
                &resolved.guardian_id,
                Some(&set_value),
                Some(&unset),
                clear_opt,
            )?,
            GuardianEnvSection::ManualChecks => client.set_guardian_manual_checks_env(
                &resolved.guardian_id,
                Some(&set_value),
                Some(&unset),
                clear_opt,
            )?,
        };
        let mut changes: Vec<(String, &'static str)> =
            set_map.keys().cloned().map(|k| (k, "set")).collect();
        let mut unset_sorted = unset.clone();
        unset_sorted.sort();
        changes.extend(unset_sorted.into_iter().map(|k| (k, "unset")));
        let mut clear_sorted = clear.clone();
        clear_sorted.sort();
        changes.extend(clear_sorted.into_iter().map(|k| (k, "cleared")));
        let label = section.label();
        let payload = Value::Array(
            changes
                .iter()
                .map(|(k, s)| serde_json::json!({"key": k, "status": s}))
                .collect(),
        );
        emit(opts, &payload, |_| {
            println!("{selector} {label} env updated:");
            for (k, s) in &changes {
                println!("  {k}: {s}");
            }
        });
        Ok(())
    })
}

fn dispatch_upstream(cmd: ReviewUpstreamCommand, opts: &GlobalOpts, client: &DaemonClient) -> i32 {
    match cmd {
        ReviewUpstreamCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["review", "upstream"])
                    .expect("review upstream help exists")
            );
            0
        }
        ReviewUpstreamCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ReviewUpstreamCommand::List { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let branches = client.guardian_base_branches(&resolved.guardian_id)?;
            emit(opts, &branches, |bs| {
                let bs = bs.as_array().cloned().unwrap_or_default();
                if bs.is_empty() {
                    println!("no candidate upstream branches");
                    return;
                }
                for b in &bs {
                    println!("{}", b.as_str().unwrap_or_default());
                }
            });
            Ok(())
        }),
        ReviewUpstreamCommand::Set { selector, branch } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_change_base(&resolved.guardian_id, &branch)?;
            emit(opts, &result, |_| {
                println!("{selector} upstream branch -> '{branch}'")
            });
            Ok(())
        }),
    }
}

#[allow(clippy::too_many_lines)] // Wide leaf set (submit/list/show/find/update/comments/pull-feedback); splitting further would scatter one cohesive group across helper fns for no real gain.
fn dispatch_pr(cmd: ReviewPrCommand, opts: &GlobalOpts, client: &DaemonClient) -> i32 {
    match cmd {
        ReviewPrCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["review", "pr"]).expect("review pr help exists")
            );
            0
        }
        ReviewPrCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ReviewPrCommand::Submit {
            selector,
            position,
            combined,
            alias,
            title,
            description,
            use_worktree_branch_name,
        } => run_and_report(opts, Some("ralphus review list --pr-ready"), || {
            let mut pr_spec = serde_json::Map::new();
            if let Some(alias) = &alias {
                pr_spec.insert("branch_alias".to_string(), Value::String(alias.clone()));
            }
            if let Some(title) = &title {
                pr_spec.insert("title".to_string(), Value::String(title.clone()));
            }
            if let Some(description) = &description {
                pr_spec.insert(
                    "description".to_string(),
                    Value::String(description.clone()),
                );
            }
            if let Some(use_worktree_branch_name) = use_worktree_branch_name {
                pr_spec.insert(
                    "use_worktree_branch_name".to_string(),
                    Value::Bool(use_worktree_branch_name),
                );
            }
            let resolved =
                resolve_guardian_selector(client, &selector, "ralphus review list --pr-ready")?;
            if !combined {
                let position = position.unwrap_or_default();
                let guardian = client.guardian_get(&resolved.guardian_id)?;
                let branches = guardian["branches"].as_array().cloned().unwrap_or_default();
                let found = branches
                    .iter()
                    .find(|b| b["position"].as_i64() == Some(position));
                let Some(found) = found else {
                    return Err(CommandError::Selector(SelectorError(format!(
                        "no branch at position {position} in this review"
                    ))));
                };
                pr_spec.insert("branch_id".to_string(), found["id"].clone());
            }
            let result =
                client.guardian_submit_prs(&resolved.guardian_id, &[Value::Object(pr_spec)])?;
            emit(opts, &result, |_| {
                println!("submitting PR for {selector}...")
            });
            Ok(())
        }),
        ReviewPrCommand::List { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_list_prs(&resolved.guardian_id)?;
            emit(opts, &result, render_pr_list);
            Ok(())
        }),
        ReviewPrCommand::Show { pr_id } => run_and_report(opts, None, || {
            let result = client.pr_get(&pr_id)?;
            emit(opts, &result, |r| {
                println!(
                    "{}: {} (#{}, {})",
                    r["id"], r["title"], r["pr_number"], r["state"]
                )
            });
            Ok(())
        }),
        ReviewPrCommand::Find {
            forge,
            repo,
            pr_number,
        } => run_and_report(opts, None, || {
            let result = client.pr_find(&forge, &repo, pr_number)?;
            emit(opts, &result, |r| {
                println!("{}", r["id"].as_str().unwrap_or_default())
            });
            Ok(())
        }),
        ReviewPrCommand::Update {
            pr_id,
            pr_number,
            pr_url,
            branch_alias,
            state,
        } => run_and_report(opts, None, || {
            let result = client.pr_update(
                &pr_id,
                pr_number,
                pr_url.as_deref(),
                branch_alias.as_deref(),
                state.as_deref(),
            )?;
            emit(opts, &result, |_| println!("{pr_id} updated"));
            Ok(())
        }),
        ReviewPrCommand::Comments { pr_id } => run_and_report(opts, None, || {
            let result = client.pr_comments(&pr_id)?;
            emit(opts, &result, render_pr_comments);
            Ok(())
        }),
        ReviewPrCommand::PullFeedback { pr_id } => run_and_report(opts, None, || {
            let result = client.pr_action_feedback(&pr_id)?;
            emit(opts, &result, |_| {
                println!("pulling feedback for {pr_id}...")
            });
            Ok(())
        }),
        ReviewPrCommand::PullFromPr { pr_id } => run_and_report(opts, None, || {
            let result = client.pr_pull_from_pr(&pr_id)?;
            emit(opts, &result, |_| {
                println!("pulling PR commits for {pr_id}...")
            });
            Ok(())
        }),
        ReviewPrCommand::Unlink { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let result = client.guardian_unlink_prs(&resolved.guardian_id)?;
            emit(opts, &result, |r| {
                println!(
                    "unlinked {} open pr(s) for {selector}",
                    r["dropped"].as_i64().unwrap_or(0)
                )
            });
            Ok(())
        }),
    }
}

fn dispatch_branch(cmd: ReviewBranchCommand, opts: &GlobalOpts, client: &DaemonClient) -> i32 {
    match cmd {
        ReviewBranchCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["review", "branch"])
                    .expect("review branch help exists")
            );
            0
        }
        ReviewBranchCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ReviewBranchCommand::Enable { selector } => {
            dispatch_branch_set_enabled(opts, client, selector, true)
        }
        ReviewBranchCommand::Disable { selector } => {
            dispatch_branch_set_enabled(opts, client, selector, false)
        }
        ReviewBranchCommand::Terminal { selector, mode } => {
            dispatch_branch_terminal(opts, client, selector, mode)
        }
    }
}

fn dispatch_branch_set_enabled(
    opts: &GlobalOpts,
    client: &DaemonClient,
    selector: String,
    enabled: bool,
) -> i32 {
    let resolved = match resolve_branch(opts, client, &selector) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let guardian = match client.guardian_get(&resolved.guardian_id) {
        Ok(g) => g,
        Err(e) => return fail_daemon(opts, e),
    };
    let order: Vec<String> = guardian["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["branch"].as_str().map(str::to_string))
        .collect();
    let mut enabled_map = serde_json::Map::new();
    if let Some(branch) = &resolved.branch {
        enabled_map.insert(branch.clone(), Value::Bool(enabled));
    }
    match client.guardian_arrange(
        &resolved.guardian_id,
        &order,
        Some(&Value::Object(enabled_map)),
    ) {
        Ok(result) => {
            let verb = if enabled { "enabled" } else { "disabled" };
            emit(opts, &result, |_| {
                println!("{selector} {verb}; rebase started")
            });
            0
        }
        Err(e) => fail_daemon(opts, e),
    }
}

/// Prints the resume command for a branch's conflict-resolver session
/// instead of calling the daemon's terminal-spawning endpoint -- see the
/// module doc comment.
fn dispatch_branch_terminal(
    opts: &GlobalOpts,
    client: &DaemonClient,
    selector: String,
    mode: String,
) -> i32 {
    let resolved = match resolve_branch(opts, client, &selector) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let guardian = match client.guardian_get(&resolved.guardian_id) {
        Ok(g) => g,
        Err(e) => return fail_daemon(opts, e),
    };
    let branch = guardian["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["id"].as_str() == resolved.branch_id.as_deref());
    let Some(branch) = branch else {
        return fail_message(opts, format!("no branch '{selector}' in this review"), 2);
    };
    let agent_session_id = branch["resolver_agent_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if agent_session_id.is_empty() {
        return fail_message(
            opts,
            format!(
                "no resolver_agent_session_id available for '{selector}' -- conflict resolution may not have run yet"
            ),
            1,
        );
    }
    let cmd = agent_resume_command(
        guardian["resolver_agent"].as_str(),
        &agent_session_id,
        &mode,
    );
    let cwd = branch["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("-")
        .to_string();
    let command_line = cmd.join(" ");
    emit(opts, branch, |_| {
        print_command_with_cwd(&cwd, &command_line, None)
    });
    0
}

fn dispatch_checks(cmd: ReviewChecksCommand, opts: &GlobalOpts, client: &DaemonClient) -> i32 {
    match cmd {
        ReviewChecksCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["review", "checks"])
                    .expect("review checks help exists")
            );
            0
        }
        ReviewChecksCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ReviewChecksCommand::List { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let guardian = client.guardian_get(&resolved.guardian_id)?;
            let commands = guardian["manual_commands"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let input_values = guardian["input_values"].clone();
            emit(opts, &Value::Array(commands.clone()), |_| {
                if commands.is_empty() {
                    println!("no manual checks available (review may still be building)");
                    return;
                }
                for (i, check) in commands.iter().enumerate() {
                    println!("[{i}] {}", check["command"]);
                    for inp in check["inputs"].as_array().into_iter().flatten() {
                        let name = inp["name"].as_str().unwrap_or_default();
                        let current = input_values[name]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                inp["default"].as_str().unwrap_or_default().to_string()
                            });
                        println!(
                            "      input {name}: {} (current: {current:?})",
                            inp["message"]
                        );
                    }
                }
            });
            Ok(())
        }),
        ReviewChecksCommand::Run {
            selector,
            index,
            all,
            input,
        } => dispatch_checks_run(opts, client, selector, index, all, input),
        ReviewChecksCommand::Terminal { selector, mode } => {
            dispatch_checks_terminal(opts, client, selector, mode)
        }
    }
}

#[allow(clippy::too_many_arguments)] // Faithfully mirrors `_cmd_review_checks_run`'s argument shape; bundling into a struct would just move the same count elsewhere.
fn dispatch_checks_run(
    opts: &GlobalOpts,
    client: &DaemonClient,
    selector: String,
    index: Vec<i64>,
    all: bool,
    input: Vec<(String, String)>,
) -> i32 {
    let resolved = match resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT) {
        Ok(r) => r,
        Err(e) => return fail_selector(opts, e),
    };
    let guardian = match client.guardian_get(&resolved.guardian_id) {
        Ok(g) => g,
        Err(e) => return fail_daemon(opts, e),
    };
    let commands = guardian["manual_commands"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if commands.is_empty() {
        return fail_message(
            opts,
            "no manual checks available (review may still be building)".to_string(),
            1,
        );
    }
    let indices: Vec<usize> = if all || index.is_empty() {
        (0..commands.len()).collect()
    } else {
        let bad: Vec<i64> = index
            .iter()
            .copied()
            .filter(|&i| i < 0 || i as usize >= commands.len())
            .collect();
        if !bad.is_empty() {
            return fail_message(
                opts,
                format!(
                    "check index out of range: {bad:?} (have {})",
                    commands.len()
                ),
                2,
            );
        }
        index.iter().map(|&i| i as usize).collect()
    };
    let cwd = guardian["combined_worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| guardian["git_root"].as_str())
        .unwrap_or_default()
        .to_string();
    let input_values = &guardian["input_values"];
    let mut resolved_commands: BTreeMap<usize, String> = BTreeMap::new();
    let mut all_missing: Vec<(usize, Vec<String>)> = Vec::new();
    for &i in &indices {
        let command = commands[i]["command"].as_str().unwrap_or_default();
        let (resolved_cmd, missing) =
            resolve_check_inputs(command, &commands[i], input_values, &input);
        resolved_commands.insert(i, resolved_cmd);
        if !missing.is_empty() {
            all_missing.push((i, missing));
        }
    }
    if !all_missing.is_empty() {
        let detail = all_missing
            .iter()
            .map(|(i, names)| format!("[{i}] needs {names:?}"))
            .collect::<Vec<_>>()
            .join("; ");
        return fail_message(
            opts,
            format!("missing required input value(s): {detail} (supply via --input NAME=VALUE)"),
            2,
        );
    }
    let env_keys: Vec<String> = {
        let mut keys: Vec<String> = guardian["manual_checks_env"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    };
    let payload = Value::Array(
        indices
            .iter()
            .map(|&i| serde_json::json!({"index": i, "command": resolved_commands[&i], "env_keys": env_keys}))
            .collect(),
    );
    emit(opts, &payload, |_| {
        for &i in &indices {
            println!("[{i}]");
            print_command_with_cwd(&cwd, &resolved_commands[&i], Some(&env_keys));
        }
    });
    0
}

/// Prints the resume command for the manual-checks-generation agent session
/// instead of calling the daemon's terminal-spawning endpoint -- see the
/// module doc comment.
fn dispatch_checks_terminal(
    opts: &GlobalOpts,
    client: &DaemonClient,
    selector: String,
    mode: String,
) -> i32 {
    let resolved = match resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT) {
        Ok(r) => r,
        Err(e) => return fail_selector(opts, e),
    };
    let guardian = match client.guardian_get(&resolved.guardian_id) {
        Ok(g) => g,
        Err(e) => return fail_daemon(opts, e),
    };
    let agent_session_id = guardian["manual_commands_agent_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if agent_session_id.is_empty() {
        return fail_message(
            opts,
            format!(
                "no manual_commands_agent_session_id available for '{selector}' -- manual-checks generation may not have run yet"
            ),
            1,
        );
    }
    let cwd = guardian["combined_worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| guardian["git_root"].as_str())
        .unwrap_or("-")
        .to_string();
    let cmd = agent_resume_command(
        guardian["manual_commands_agent"].as_str(),
        &agent_session_id,
        &mode,
    );
    let command_line = cmd.join(" ");
    emit(opts, &guardian, |_| {
        print_command_with_cwd(&cwd, &command_line, None)
    });
    0
}

fn dispatch_action(cmd: ReviewActionCommand, opts: &GlobalOpts, client: &DaemonClient) -> i32 {
    match cmd {
        ReviewActionCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["review", "action"])
                    .expect("review action help exists")
            );
            0
        }
        ReviewActionCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ReviewActionCommand::List { selector } => run_and_report(opts, None, || {
            let resolved = resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT)?;
            let guardian = client.guardian_get(&resolved.guardian_id)?;
            let hints = guardian["action_hints"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let input_values = guardian["input_values"].clone();
            emit(opts, &Value::Array(hints.clone()), |_| {
                if hints.is_empty() {
                    println!("no action hints declared");
                    return;
                }
                for (i, h) in hints.iter().enumerate() {
                    let kind = if h["command"].as_str().filter(|s| !s.is_empty()).is_some() {
                        "command"
                    } else {
                        "prompt"
                    };
                    println!("[{i}] {}  ({kind})", h["label"]);
                    for inp in h["inputs"].as_array().into_iter().flatten() {
                        let name = inp["name"].as_str().unwrap_or_default();
                        let current = input_values[name]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                inp["default"].as_str().unwrap_or_default().to_string()
                            });
                        println!(
                            "      input {name}: {} (current: {current:?})",
                            inp["message"]
                        );
                    }
                }
            });
            Ok(())
        }),
        ReviewActionCommand::Run {
            selector,
            index,
            input,
        } => dispatch_action_run(opts, client, selector, index, input),
    }
}

/// Prints the resolved command + cwd for a command-kind action hint rather
/// than calling the daemon's terminal-spawning endpoint (same rationale as
/// `dispatch_checks_run`). A prompt-kind hint has no headless run path at
/// all yet (the daemon itself returns 501 for these), so it is rejected here
/// too.
fn dispatch_action_run(
    opts: &GlobalOpts,
    client: &DaemonClient,
    selector: String,
    index: i64,
    input: Vec<(String, String)>,
) -> i32 {
    let resolved = match resolve_guardian_selector(client, &selector, DEFAULT_REVIEW_LIST_HINT) {
        Ok(r) => r,
        Err(e) => return fail_selector(opts, e),
    };
    let guardian = match client.guardian_get(&resolved.guardian_id) {
        Ok(g) => g,
        Err(e) => return fail_daemon(opts, e),
    };
    let hints = guardian["action_hints"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if index < 0 || index as usize >= hints.len() {
        return fail_message(
            opts,
            format!(
                "action hint index {index} out of range (have {})",
                hints.len()
            ),
            2,
        );
    }
    let hint = &hints[index as usize];
    let command = hint["command"].as_str().unwrap_or_default();
    if command.is_empty() {
        return fail_message(
            opts,
            "prompt-kind action hints cannot be run directly yet".to_string(),
            1,
        );
    }
    let cwd = guardian["combined_worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| guardian["git_root"].as_str())
        .unwrap_or_default()
        .to_string();
    let input_values = &guardian["input_values"];
    let (resolved_command, missing) = resolve_check_inputs(command, hint, input_values, &input);
    if !missing.is_empty() {
        return fail_message(
            opts,
            format!("missing required input value(s): {missing:?} (supply via --input NAME=VALUE)"),
            2,
        );
    }
    let mut hint_with_command = hint.clone();
    if let Value::Object(map) = &mut hint_with_command {
        map.insert(
            "command".to_string(),
            Value::String(resolved_command.clone()),
        );
    }
    emit(opts, &hint_with_command, |_| {
        print_command_with_cwd(&cwd, &resolved_command, None)
    });
    0
}

// ---- rendering -------------------------------------------------------------

fn render_review_list(guardians: &Value) {
    let list = guardians.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no reviews");
        return;
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|g| {
            vec![
                g["id"].as_str().unwrap_or_default().to_string(),
                g["name"].as_str().unwrap_or_default().to_string(),
                g["status"].as_str().unwrap_or_default().to_string(),
                g["base_branch"].as_str().unwrap_or_default().to_string(),
            ]
        })
        .collect();
    crate::output::print_table(&["ID", "NAME", "STATUS", "BASE_BRANCH"], &rows);
}

#[allow(clippy::too_many_lines)] // One flat field-by-field kv render, mirroring `_cmd_review_show`'s own `_render`; splitting it up would just scatter one view across artificial helper fns.
fn render_review_detail(g: &Value) {
    let mp = &g["merge_progress"];
    let proof_scope = g["proof_scope"].as_str();
    let proof_skip_auto_clean = g["proof_skip_auto_clean"].as_bool();
    let mut rows: Vec<(&str, String)> = Vec::new();
    if let Some(uri) = g["uri"].as_str() {
        rows.push(("uri", uri.to_string()));
    }
    rows.push(("id", g["id"].as_str().unwrap_or_default().to_string()));
    rows.push(("name", g["name"].as_str().unwrap_or_default().to_string()));
    rows.push((
        "status",
        g["status"].as_str().unwrap_or_default().to_string(),
    ));
    rows.push(("ready", g["ready"].to_string()));
    rows.push((
        "review_type",
        g["review_type"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("git")
            .to_string(),
    ));
    rows.push((
        "base_branch",
        g["base_branch"].as_str().unwrap_or_default().to_string(),
    ));
    rows.push((
        "review_branch",
        g["review_branch"].as_str().unwrap_or_default().to_string(),
    ));
    rows.push((
        "git_root",
        g["git_root"].as_str().unwrap_or_default().to_string(),
    ));
    rows.push((
        "combined_worktree",
        g["combined_worktree"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("-")
            .to_string(),
    ));
    rows.push((
        "merge_progress",
        format!(
            "{}/{}",
            mp["done"].as_i64().unwrap_or(0),
            mp["total"].as_i64().unwrap_or(0)
        ),
    ));
    rows.push(("conflicts_found", value_or_dash(&g["conflicts_found"])));
    rows.push(("conflicts_fixed", value_or_dash(&g["conflicts_fixed"])));
    rows.push((
        "conflicts_committed",
        value_or_dash(&g["conflicts_committed"]),
    ));
    rows.push((
        "resolver_agent",
        g["resolver_agent"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("(default)")
            .to_string(),
    ));
    rows.push((
        "resolver_model",
        g["resolver_model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("(default)")
            .to_string(),
    ));
    rows.push((
        "proof_scope",
        match proof_scope {
            Some(v) if !v.is_empty() => format!("{v} (explicit)"),
            _ => "(inherited from project default)".to_string(),
        },
    ));
    rows.push((
        "effective_proof_scope",
        g["effective_proof_scope"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    ));
    rows.push((
        "proof_skip_auto_clean",
        match proof_skip_auto_clean {
            Some(v) => format!("{v} (explicit)"),
            None => "(inherited from project default)".to_string(),
        },
    ));
    rows.push((
        "effective_proof_skip_auto_clean",
        g["effective_proof_skip_auto_clean"].to_string(),
    ));
    rows.push((
        "skip_auto_build",
        g["skip_auto_build"].as_bool().unwrap_or(false).to_string(),
    ));
    rows.push((
        "skip_worktrees",
        g["skip_worktrees"].as_bool().unwrap_or(false).to_string(),
    ));
    let squash_projects: Vec<String> = g["squash_projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    rows.push((
        "squash_projects",
        if squash_projects.is_empty() {
            "-".to_string()
        } else {
            squash_projects.join(", ")
        },
    ));
    rows.push((
        "auto_pr_feedback",
        g["auto_pr_feedback"].as_bool().unwrap_or(false).to_string(),
    ));
    rows.push((
        "summary_state",
        g["summary_state"].as_str().unwrap_or_default().to_string(),
    ));
    if g["summary_state"].as_str() == Some("ready") {
        rows.push((
            "summary_agent",
            g["summary_agent"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("(default)")
                .to_string(),
        ));
        rows.push((
            "summary_model",
            g["summary_model"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("(default)")
                .to_string(),
        ));
        rows.push((
            "change_summary",
            g["change_summary"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("-")
                .to_string(),
        ));
    }
    crate::output::print_kv(&rows);

    let branches = g["branches"].as_array().cloned().unwrap_or_default();
    if !branches.is_empty() {
        println!("\nbranches:");
        for b in &branches {
            let detail = b["detail"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|d| format!("  ({d})"))
                .unwrap_or_default();
            let ready = if b["ready"].as_bool().unwrap_or(false) {
                "  ready"
            } else {
                ""
            };
            println!(
                "  [{}] {}  {}{ready}{detail}",
                b["position"], b["branch"], b["merge_status"]
            );
        }
    }
    let manual_commands = g["manual_commands"].as_array().cloned().unwrap_or_default();
    if !manual_commands.is_empty() {
        println!("\nmanual checks:");
        for (i, check) in manual_commands.iter().enumerate() {
            println!("  [{i}] {}", check["command"]);
        }
    }
    print_guardian_env_summary(g);
}

fn render_review_status(g: &Value) {
    let branches = g["branches"].as_array().cloned().unwrap_or_default();
    let rows: Vec<Vec<String>> = branches
        .iter()
        .map(|b| {
            vec![
                b["branch"].as_str().unwrap_or_default().to_string(),
                b["merge_status"].as_str().unwrap_or_default().to_string(),
                if b["ready"].as_bool().unwrap_or(false) {
                    "ready".to_string()
                } else {
                    "-".to_string()
                },
                b["detail"].as_str().unwrap_or_default().to_string(),
            ]
        })
        .collect();
    crate::output::print_table(&["BRANCH", "MERGE_STATUS", "READY", "DETAIL"], &rows);
    let mp = &g["merge_progress"];
    let done = mp["done"].as_i64().unwrap_or(0);
    let total = mp["total"].as_i64().unwrap_or(0);
    let pct = mp["pct"].as_f64().unwrap_or(0.0);
    println!("\nmerged: {done}/{total} ({pct:.1}%)");
    println!("verdict: {}", review_status_verdict(g));
}

fn render_review_worktrees(g: &Value) {
    let branches = g["branches"].as_array().cloned().unwrap_or_default();
    if branches.is_empty() {
        println!("no branches");
        return;
    }
    let rows: Vec<Vec<String>> = branches
        .iter()
        .map(|b| {
            let source = match b["source_squad_id"].as_str() {
                Some(r) => format!("{r}/{}/{}", b["source_task_idx"], b["source_cell_idx"]),
                None => "-".to_string(),
            };
            vec![
                b["position"]
                    .as_i64()
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
                b["branch"].as_str().unwrap_or_default().to_string(),
                b["worktree"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("-")
                    .to_string(),
                source,
            ]
        })
        .collect();
    crate::output::print_table(&["POS", "BRANCH", "WORKTREE", "SOURCE"], &rows);
}

fn render_events(events: &Value) {
    let events = events.as_array().cloned().unwrap_or_default();
    if events.is_empty() {
        println!("no events");
        return;
    }
    for e in &events {
        let ref_str = e["ref"]
            .as_str()
            .map(|r| format!(" {r}"))
            .unwrap_or_default();
        println!("[{}] {}{ref_str}: {}", e["at_ms"], e["scope"], e["message"]);
    }
}

fn render_pr_list(rows: &Value) {
    let rows = rows.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no PRs submitted yet");
        return;
    }
    for r in &rows {
        let pos_label = if r["branch_id"].is_null() {
            "combined".to_string()
        } else {
            format!("branch {}", r["branch_id"])
        };
        println!(
            "{}  {pos_label}  #{}  {}  {}",
            r["id"], r["pr_number"], r["state"], r["title"]
        );
    }
}

fn render_pr_comments(rows: &Value) {
    let rows = rows.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no comments");
        return;
    }
    for c in &rows {
        let tag = if c["actioned"].as_bool().unwrap_or(false) {
            "actioned"
        } else {
            "new"
        };
        println!("[{tag}] {}: {}", c["author"], c["body"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_review_is_help() {
        matches!(parse(&v(&[])), ReviewCommand::Help);
    }

    #[test]
    fn parses_env_with_and_without_a_scope() {
        match parse(&v(&["env", "g1"])) {
            ReviewCommand::Env { selector, scope } => {
                assert_eq!(selector, "g1");
                assert_eq!(
                    scope, None,
                    "the default is decided once the selector resolves"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        for want in ["worktree", "build", "tests", "manual-checks"] {
            match parse(&v(&["env", "g1", "--scope", want])) {
                ReviewCommand::Env { scope, .. } => assert_eq!(scope.as_deref(), Some(want)),
                other => panic!("unexpected for {want}: {other:?}"),
            }
        }
        matches!(
            parse(&v(&["env", "g1", "--scope", "nope"])),
            ReviewCommand::UsageError(_)
        );
        matches!(parse(&v(&["env"])), ReviewCommand::UsageError(_));
    }

    #[test]
    fn parses_list_with_flags() {
        match parse(&v(&[
            "list",
            "--status",
            "collecting,in_review",
            "--pr-ready",
        ])) {
            ReviewCommand::List { status, pr_ready } => {
                assert_eq!(status.as_deref(), Some("collecting,in_review"));
                assert!(pr_ready);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_show_with_selector() {
        match parse(&v(&["show", "@my-review"])) {
            ReviewCommand::Show { selector } => assert_eq!(selector, "@my-review"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_selector_is_usage_error() {
        matches!(parse(&v(&["show"])), ReviewCommand::UsageError(_));
    }

    #[test]
    fn parses_create_positionals_and_flags() {
        match parse(&v(&[
            "create",
            "my review",
            "main",
            "/repo",
            "--checks",
            "fmt,test",
            "--skip-auto-build",
            "--review-type",
            "hotfix",
        ])) {
            ReviewCommand::Create {
                name,
                base_branch,
                git_root,
                checks,
                skip_auto_build,
                review_type,
                ..
            } => {
                assert_eq!(name, "my review");
                assert_eq!(base_branch, "main");
                assert_eq!(git_root, "/repo");
                assert_eq!(checks.as_deref(), Some("fmt,test"));
                assert!(skip_auto_build);
                assert_eq!(review_type.as_deref(), Some("hotfix"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn create_requires_three_positionals() {
        matches!(
            parse(&v(&["create", "name", "main"])),
            ReviewCommand::UsageError(_)
        );
    }

    #[test]
    fn delete_parses_yes_flag() {
        match parse(&v(&["delete", "g1", "--yes"])) {
            ReviewCommand::Delete { selector, yes } => {
                assert_eq!(selector, "g1");
                assert!(yes);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_settings_tri_state_flags() {
        match parse(&v(&[
            "settings",
            "g1",
            "--skip-auto-build",
            "--no-skip-worktrees",
            "--proof-scope",
            "each_branch",
        ])) {
            ReviewCommand::Settings {
                selector,
                skip_auto_build,
                skip_worktrees,
                proof_scope,
                ..
            } => {
                assert_eq!(selector, "g1");
                assert_eq!(skip_auto_build, Some(true));
                assert_eq!(skip_worktrees, Some(false));
                assert_eq!(proof_scope.as_deref(), Some("each_branch"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn settings_tri_state_defaults_to_none_when_absent() {
        match parse(&v(&["settings", "g1"])) {
            ReviewCommand::Settings {
                skip_auto_build,
                skip_worktrees,
                skip_base_updates,
                ..
            } => {
                assert_eq!(skip_auto_build, None);
                assert_eq!(skip_worktrees, None);
                assert_eq!(skip_base_updates, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_settings_skip_base_updates_tri_state() {
        // Both the positive opt-out and its --no- negative parse to the
        // expected tri-state values (RAL-250).
        match parse(&v(&["settings", "g1", "--skip-base-updates"])) {
            ReviewCommand::Settings {
                skip_base_updates, ..
            } => assert_eq!(skip_base_updates, Some(true)),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["settings", "g1", "--no-skip-base-updates"])) {
            ReviewCommand::Settings {
                skip_base_updates, ..
            } => assert_eq!(skip_base_updates, Some(false)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_settings_match_pr_branch_name_tri_state() {
        match parse(&v(&["settings", "g1", "--match-pr-branch-name"])) {
            ReviewCommand::Settings {
                match_pr_branch_name,
                ..
            } => assert_eq!(match_pr_branch_name, Some(true)),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["settings", "g1", "--no-match-pr-branch-name"])) {
            ReviewCommand::Settings {
                match_pr_branch_name,
                ..
            } => assert_eq!(match_pr_branch_name, Some(false)),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["settings", "g1"])) {
            ReviewCommand::Settings {
                match_pr_branch_name,
                ..
            } => assert_eq!(match_pr_branch_name, None),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_build_env_repeated_flags() {
        match parse(&v(&[
            "build-env",
            "g1",
            "--set",
            "A=1",
            "--set",
            "B=2",
            "--unset",
            "C",
            "--clear",
            "D",
        ])) {
            ReviewCommand::BuildEnv(args) => {
                assert_eq!(args.selector, "g1");
                assert_eq!(args.set, vec!["A=1".to_string(), "B=2".to_string()]);
                assert_eq!(args.unset, vec!["C".to_string()]);
                assert_eq!(args.clear, vec!["D".to_string()]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_manual_checks_env() {
        match parse(&v(&["manual-checks-env", "g1", "--set", "A=1"])) {
            ReviewCommand::ManualChecksEnv(args) => assert_eq!(args.selector, "g1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_squash_on() {
        match parse(&v(&["squash", "g1", "proj", "--on"])) {
            ReviewCommand::Squash {
                selector,
                project,
                enabled,
            } => {
                assert_eq!(selector, "g1");
                assert_eq!(project, "proj");
                assert!(enabled);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn squash_requires_on_or_off() {
        matches!(
            parse(&v(&["squash", "g1", "proj"])),
            ReviewCommand::UsageError(_)
        );
    }

    #[test]
    fn parses_add_branch() {
        match parse(&v(&["add-branch", "g1", "feature/x"])) {
            ReviewCommand::AddBranch { selector, branch } => {
                assert_eq!(selector, "g1");
                assert_eq!(branch, "feature/x");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_sync_pr() {
        match parse(&v(&["sync-pr", "g1"])) {
            ReviewCommand::SyncPr { selector } => assert_eq!(selector, "g1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_reorder_with_disable_enable() {
        match parse(&v(&[
            "reorder",
            "g1",
            "a,b,c",
            "--disable",
            "a",
            "--enable",
            "b",
        ])) {
            ReviewCommand::Reorder {
                selector,
                order,
                disable,
                enable,
            } => {
                assert_eq!(selector, "g1");
                assert_eq!(order, "a,b,c");
                assert_eq!(disable.as_deref(), Some("a"));
                assert_eq!(enable.as_deref(), Some("b"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_feedback_positional_pair() {
        match parse(&v(&["feedback", "g1~0", "please fix"])) {
            ReviewCommand::Feedback { selector, text } => {
                assert_eq!(selector, "g1~0");
                assert_eq!(text, "please fix");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_move_branch() {
        match parse(&v(&["move-branch", "g1~0", "g2"])) {
            ReviewCommand::MoveBranch {
                selector,
                to_review,
            } => {
                assert_eq!(selector, "g1~0");
                assert_eq!(to_review, "g2");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_upstream_list() {
        match parse(&v(&["upstream", "list", "g1"])) {
            ReviewCommand::Upstream(ReviewUpstreamCommand::List { selector }) => {
                assert_eq!(selector, "g1");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_upstream_set() {
        match parse(&v(&["upstream", "set", "g1", "main"])) {
            ReviewCommand::Upstream(ReviewUpstreamCommand::Set { selector, branch }) => {
                assert_eq!(selector, "g1");
                assert_eq!(branch, "main");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn bare_upstream_is_help() {
        matches!(
            parse(&v(&["upstream"])),
            ReviewCommand::Upstream(ReviewUpstreamCommand::Help)
        );
    }

    #[test]
    fn parses_pr_submit_by_position() {
        match parse(&v(&[
            "pr",
            "submit",
            "g1",
            "--position",
            "1",
            "--alias",
            "my-branch",
        ])) {
            ReviewCommand::Pr(ReviewPrCommand::Submit {
                selector,
                position,
                combined,
                alias,
                ..
            }) => {
                assert_eq!(selector, "g1");
                assert_eq!(position, Some(1));
                assert!(!combined);
                assert_eq!(alias.as_deref(), Some("my-branch"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_pr_submit_use_worktree_branch_name() {
        match parse(&v(&[
            "pr",
            "submit",
            "g1",
            "--combined",
            "--use-worktree-branch-name",
        ])) {
            ReviewCommand::Pr(ReviewPrCommand::Submit {
                use_worktree_branch_name,
                ..
            }) => {
                assert_eq!(use_worktree_branch_name, Some(true));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn pr_submit_use_worktree_branch_name_defaults_to_none() {
        match parse(&v(&["pr", "submit", "g1", "--combined"])) {
            ReviewCommand::Pr(ReviewPrCommand::Submit {
                use_worktree_branch_name,
                ..
            }) => {
                assert_eq!(use_worktree_branch_name, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_pr_submit_combined() {
        match parse(&v(&["pr", "submit", "g1", "--combined"])) {
            ReviewCommand::Pr(ReviewPrCommand::Submit {
                combined, position, ..
            }) => {
                assert!(combined);
                assert_eq!(position, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn pr_submit_rejects_both_position_and_combined() {
        matches!(
            parse(&v(&["pr", "submit", "g1", "--position", "0", "--combined"])),
            ReviewCommand::Pr(ReviewPrCommand::UsageError(_))
        );
    }

    #[test]
    fn pr_submit_rejects_neither_position_nor_combined() {
        matches!(
            parse(&v(&["pr", "submit", "g1"])),
            ReviewCommand::Pr(ReviewPrCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_pr_find() {
        match parse(&v(&["pr", "find", "github", "owner/repo", "42"])) {
            ReviewCommand::Pr(ReviewPrCommand::Find {
                forge,
                repo,
                pr_number,
            }) => {
                assert_eq!(forge, "github");
                assert_eq!(repo, "owner/repo");
                assert_eq!(pr_number, 42);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_pr_update_flags() {
        match parse(&v(&[
            "pr",
            "update",
            "pr-1",
            "--pr-number",
            "7",
            "--state",
            "merged",
        ])) {
            ReviewCommand::Pr(ReviewPrCommand::Update {
                pr_id,
                pr_number,
                state,
                ..
            }) => {
                assert_eq!(pr_id, "pr-1");
                assert_eq!(pr_number, Some(7));
                assert_eq!(state.as_deref(), Some("merged"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_pr_comments_and_pull_feedback() {
        matches!(
            parse(&v(&["pr", "comments", "pr-1"])),
            ReviewCommand::Pr(ReviewPrCommand::Comments { .. })
        );
        matches!(
            parse(&v(&["pr", "pull-feedback", "pr-1"])),
            ReviewCommand::Pr(ReviewPrCommand::PullFeedback { .. })
        );
    }

    #[test]
    fn parses_pr_pull_from_pr() {
        assert!(matches!(
            parse(&v(&["pr", "pull-from-pr", "pr-1"])),
            ReviewCommand::Pr(ReviewPrCommand::PullFromPr { .. })
        ));
    }

    #[test]
    fn parses_pr_unlink() {
        match parse(&v(&["pr", "unlink", "g1"])) {
            ReviewCommand::Pr(ReviewPrCommand::Unlink { selector }) => {
                assert_eq!(selector, "g1");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn pr_unlink_requires_a_selector() {
        assert!(matches!(
            parse(&v(&["pr", "unlink"])),
            ReviewCommand::Pr(ReviewPrCommand::UsageError(_))
        ));
    }

    #[test]
    fn parses_branch_enable_disable() {
        matches!(
            parse(&v(&["branch", "enable", "g1~0"])),
            ReviewCommand::Branch(ReviewBranchCommand::Enable { .. })
        );
        matches!(
            parse(&v(&["branch", "disable", "g1~0"])),
            ReviewCommand::Branch(ReviewBranchCommand::Disable { .. })
        );
    }

    #[test]
    fn parses_branch_terminal_default_mode() {
        match parse(&v(&["branch", "terminal", "g1~0"])) {
            ReviewCommand::Branch(ReviewBranchCommand::Terminal { selector, mode }) => {
                assert_eq!(selector, "g1~0");
                assert_eq!(mode, "open");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn branch_terminal_rejects_invalid_mode() {
        matches!(
            parse(&v(&["branch", "terminal", "g1~0", "--mode", "bogus"])),
            ReviewCommand::Branch(ReviewBranchCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_checks_list() {
        matches!(
            parse(&v(&["checks", "list", "g1"])),
            ReviewCommand::Checks(ReviewChecksCommand::List { .. })
        );
    }

    #[test]
    fn parses_checks_run_with_indices_and_inputs() {
        match parse(&v(&[
            "checks",
            "run",
            "g1",
            "--index",
            "0",
            "--index",
            "2",
            "--input",
            "NAME=value",
        ])) {
            ReviewCommand::Checks(ReviewChecksCommand::Run {
                selector,
                index,
                all,
                input,
            }) => {
                assert_eq!(selector, "g1");
                assert_eq!(index, vec![0, 2]);
                assert!(!all);
                assert_eq!(input, vec![("NAME".to_string(), "value".to_string())]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn checks_run_rejects_malformed_input() {
        matches!(
            parse(&v(&["checks", "run", "g1", "--input", "no-equals-sign"])),
            ReviewCommand::Checks(ReviewChecksCommand::UsageError(_))
        );
    }

    #[test]
    fn parses_checks_terminal() {
        match parse(&v(&["checks", "terminal", "g1", "--mode", "readonly"])) {
            ReviewCommand::Checks(ReviewChecksCommand::Terminal { mode, .. }) => {
                assert_eq!(mode, "readonly")
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_action_list_and_run() {
        matches!(
            parse(&v(&["action", "list", "g1"])),
            ReviewCommand::Action(ReviewActionCommand::List { .. })
        );
        match parse(&v(&["action", "run", "g1", "--index", "3"])) {
            ReviewCommand::Action(ReviewActionCommand::Run {
                selector,
                index,
                input,
            }) => {
                assert_eq!(selector, "g1");
                assert_eq!(index, 3);
                assert!(input.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn action_run_requires_index() {
        matches!(
            parse(&v(&["action", "run", "g1"])),
            ReviewCommand::Action(ReviewActionCommand::UsageError(_))
        );
    }

    #[test]
    fn unknown_review_subcommand_is_usage_error() {
        matches!(parse(&v(&["bogus"])), ReviewCommand::UsageError(_));
    }

    #[test]
    fn unknown_nested_subcommands_are_usage_errors() {
        matches!(
            parse(&v(&["pr", "bogus"])),
            ReviewCommand::Pr(ReviewPrCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["branch", "bogus"])),
            ReviewCommand::Branch(ReviewBranchCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["checks", "bogus"])),
            ReviewCommand::Checks(ReviewChecksCommand::UsageError(_))
        );
        matches!(
            parse(&v(&["action", "bogus"])),
            ReviewCommand::Action(ReviewActionCommand::UsageError(_))
        );
    }

    #[test]
    fn resolve_check_inputs_prefers_overrides_then_stored_then_default() {
        let check = serde_json::json!({"inputs": [{"name": "x", "default": "def"}]});
        let input_values = serde_json::json!({"x": "stored"});
        let (resolved, missing) = resolve_check_inputs("run {x}", &check, &input_values, &[]);
        assert_eq!(resolved, "run stored");
        assert!(missing.is_empty());

        let (resolved, _) = resolve_check_inputs(
            "run {x}",
            &check,
            &input_values,
            &[("x".to_string(), "override".to_string())],
        );
        assert_eq!(resolved, "run override");

        let (resolved, missing) = resolve_check_inputs("run {x}", &check, &Value::Null, &[]);
        assert_eq!(resolved, "run def");
        assert!(missing.is_empty());
    }

    #[test]
    fn resolve_check_inputs_reports_missing_when_no_source_and_empty_default() {
        let check = serde_json::json!({"inputs": [{"name": "x", "default": ""}]});
        let (_, missing) = resolve_check_inputs("run {x}", &check, &Value::Null, &[]);
        assert_eq!(missing, vec!["x".to_string()]);
    }

    #[test]
    fn is_pr_ready_requires_status_and_fully_merged_progress() {
        let ready = serde_json::json!({"status": "in_review", "merge_progress": {"total": 2, "done": 2, "failed": 0}});
        assert!(is_pr_ready(&ready));
        let collecting = serde_json::json!({"status": "collecting", "merge_progress": {"total": 2, "done": 2, "failed": 0}});
        assert!(!is_pr_ready(&collecting));
        let partial = serde_json::json!({"status": "in_review", "merge_progress": {"total": 2, "done": 1, "failed": 0}});
        assert!(!is_pr_ready(&partial));
    }

    #[test]
    fn parse_environment_flags_rejects_malformed_entry() {
        assert!(parse_environment_flags(&["no-equals".to_string()], "--set").is_err());
        let ok = parse_environment_flags(&["A=1".to_string(), "B=2".to_string()], "--set").unwrap();
        assert_eq!(ok.get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn agent_resume_command_matches_session_rs_behavior() {
        assert_eq!(
            agent_resume_command(Some("codex"), "s1", "open"),
            vec!["codex", "resume", "s1"]
        );
        assert_eq!(
            agent_resume_command(Some("pi"), "s1", "open"),
            vec!["pi", "--session", "s1", "--approve"]
        );
        assert_eq!(
            agent_resume_command(Some("claude"), "s1", "open"),
            vec!["claude", "--resume", "s1"]
        );
        assert!(
            agent_resume_command(Some("claude"), "s1", "readonly")
                .contains(&"--append-system-prompt".to_string())
        );
    }
}
