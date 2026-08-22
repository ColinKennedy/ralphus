//! `ralphus show help-map`, ported from `cli/src/ralphus/helpmap.py` (RAL-110).
//!
//! This crate has no `argparse` tree to introspect (see
//! `cli-rs/src/flags.rs`'s hand-rolled `Scanner`), so the command surface is
//! hand-encoded as a `const`/`static` tree ([`ROOT`]) built directly from two
//! sources:
//!
//! - **Which commands/flags exist** comes from this crate's own
//!   `cli-rs/src/commands/*.rs` (`parse()` functions and `Command`/
//!   `*Command` enums) -- the authoritative list, since a handful of things
//!   are simplified or missing relative to Python (see the per-node comments
//!   below for each case: `graph`'s `--global`/`--format` collapsed into
//!   plain `--dot`/`--all`, `history`'s `--live`/`--wait-until-valid` not
//!   ported, `retry`'s `--environment`/`--env-file` overrides not ported,
//!   `configuration show`'s `--no-local` flattened away, `completion`'s shell
//!   argument not ported).
//! - **One-line descriptions and flag semantics** are sourced from
//!   `cli/src/ralphus/__main__.py::build_parser()`'s own `help=`/`description`
//!   text, matched up by subcommand path.
//!
//! Two structural simplifications relative to Python's tree, both disclosed
//! per-node below:
//! - `configuration show --no-local` (Python) has no nested `show` subcommand
//!   in this crate (`Command::Configuration` is a single leaf) -- the tree
//!   reflects that flattening rather than inventing a `show` child.
//! - `task show-tutor` (Python) has no equivalent under `task` here; this
//!   crate hoists that behavior to a top-level `ralphus tutor` command
//!   (`Command::TutorShow`) instead, so the tree has a top-level `tutor` leaf
//!   rather than a `task` child.
//!
//! Chip formatting: every positional's chip is `name [type]` (declared
//! order) -- `[type]` is `str`/`integer`/`float`/`path`, with `, optional`
//! appended for an optional positional and a repeatable positional spelled
//! `name [type...]`; an optional flag that takes a value gets a `[hint]`
//! suffix (`--status [states]`), a boolean flag gets none, a repeatable flag
//! gets `[value...]`, and a `--flag/--no-flag` tri-state (this crate's
//! `take_tri_bool`, mirroring Python's `argparse.BooleanOptionalAction`) is
//! rendered as one combined chip.
//!
//! `author` and `quick-start` are excluded entirely, mirroring Python's own
//! `_HIDDEN_COMMANDS` -- neither is meant for an AI agent already driving
//! `ralphus` via this help-map to invoke on itself.

#![allow(clippy::print_stdout)] // Guidance text is this module's product, like commands/mod.rs.

/// One node in the command tree: a command or subcommand, its own chips, and
/// its children. All fields are `'static` so the whole tree is one `const`.
#[derive(Debug, Clone, Copy)]
pub struct HelpNode {
    pub name: &'static str,
    /// Positional chips, in declared order.
    pub positionals: &'static [&'static str],
    /// Optional-flag chips; alphabetized at render time (not required to be
    /// pre-sorted here).
    pub options: &'static [&'static str],
    pub description: &'static str,
    pub children: &'static [HelpNode],
    /// Trailing `(subagent)` tag -- see [`SUBAGENT_NOTE`].
    pub subagent: bool,
    /// Leading `(read-only-safe)` tag -- see [`READ_ONLY_NOTE`].
    pub read_only_safe: bool,
}

/// Builds a [`HelpNode`] -- a `const fn` so the whole tree below can be one
/// `const` literal.
const fn node(
    name: &'static str,
    positionals: &'static [&'static str],
    options: &'static [&'static str],
    description: &'static str,
    subagent: bool,
    read_only_safe: bool,
    children: &'static [HelpNode],
) -> HelpNode {
    HelpNode {
        name,
        positionals,
        options,
        description,
        children,
        subagent,
        read_only_safe,
    }
}

// ---- guidance notes (verbatim from cli/src/ralphus/helpmap.py) -----------

pub const SUBAGENT_NOTE: &str = "Commands tagged `(subagent)` below are slow, blocking, or \
otherwise best run inside a subagent (e.g. Claude Code's Task tool) rather than directly in \
your main context -- everything else is cheap enough to invoke directly.";

pub const READ_ONLY_NOTE: &str = "Commands tagged `(read-only-safe)` below (the tag appears \
before the command name, never as one of its own flags) perform no mutation -- they are the \
commands a `quick-start manager --read-only` / `quick-start reviewer --read-only` session (see \
`ralphus quick-start manager --help` / `ralphus quick-start reviewer --help`) is restricted to. \
Everything else below is assumed mutating unless proven otherwise.";

pub const PROJECT_LOOKUP_NOTE: &str = "A user will often name a project instead of giving a \
path, e.g. \"in {project_a}, do X\", \"add a new feature to {project_b}\", \"fix {project_c}\" \
-- {project_a}/{project_b}/{project_c} each stand in for whatever project name the user \
actually says, not a literal value to type. When that happens, run `ralphus project get \
<project name>` (substituting the real name) to resolve it to its on-disk path before acting.";

pub const SUBMIT_VALIDATE_NOTE: &str = "Before running `submit` on a TOML file you just wrote \
or edited, validate it first with `ralphus validate <file>` -- a fast, fully offline check (no \
daemon needed) that reports every error with its line number, so you can fix a draft in a tight \
edit loop. `submit` itself also validates before submitting by default and refuses invalid \
TOML, but that only happens once `submit` runs and needs the daemon reachable; validating first \
catches the same errors sooner and more cheaply.";

pub const SUBMIT_REVIEW_NOTE: &str = "Unless the user explicitly asks for a different split, \
author TOML so each `ralphus submit` call produces ONE Guardian review. Multiple reviews are \
fine when intentional; otherwise keep one shared `ralphus:new-review/<key>` across the file(s) \
in that submit instead of fragmenting the batch into per-task reviews.";

pub const JSON_NOTE: &str = "`--json` (emit raw daemon JSON instead of human-readable text) \
works for every command below, even though it's only listed on the root `ralphus` line of the \
tree -- it's a global flag, not a per-command one. Unlike other global flags, it works BOTH \
before and after the subcommand: `ralphus --json status` and `ralphus status --json` are \
equivalent.";

// ---- review subgroups (defined separately to keep REVIEW_CHILDREN readable) --

const REVIEW_BASE_CHILDREN: &[HelpNode] = &[
    node(
        "list",
        &["selector [str]"],
        &[],
        "List candidate base branches.",
        false,
        true, // ("review", "base", "list")
        &[],
    ),
    node(
        "set",
        &["selector [str]", "branch [str]"],
        &[],
        "Change the base branch.",
        false,
        false,
        &[],
    ),
];

const REVIEW_PR_CHILDREN: &[HelpNode] = &[
    node(
        "comments",
        &["pr_id [str]"],
        &[],
        "List a PR's comments/notes.",
        false,
        true, // ("review", "pr", "comments")
        &[],
    ),
    node(
        "find",
        &["forge [github|gitlab]", "repo [str]", "pr_number [integer]"],
        &[],
        "Look up the ralphus PR row for a forge PR/MR number.",
        false,
        true, // ("review", "pr", "find")
        &[],
    ),
    node(
        "list",
        &["selector [str]"],
        &[],
        "List PRs submitted for a review.",
        false,
        true, // ("review", "pr", "list")
        &[],
    ),
    node(
        "pull-feedback",
        &["pr_id [str]"],
        &[],
        "Action a PR's un-actioned feedback into the owning review worktree.",
        false,
        false,
        &[],
    ),
    node(
        "show",
        &["pr_id [str]"],
        &[],
        "Show one PR row.",
        false,
        true, // ("review", "pr", "show")
        &[],
    ),
    node(
        "submit",
        &["selector [str]"],
        &[
            "--alias [name]",
            "--combined",
            "--description [text]",
            "--position [integer]",
            "--title [text]",
        ],
        "Submit a PR/MR for one stacked branch or the combined worktree.",
        false,
        false,
        &[],
    ),
    node(
        "update",
        &["pr_id [str]"],
        &[
            "--branch-alias [name]",
            "--pr-number [integer]",
            "--pr-url [url]",
            "--state [open|merged|closed]",
        ],
        "Mutate the recorded PR mapping, e.g. after a PR is closed and reopened under a new \
number.",
        false,
        false,
        &[],
    ),
];

const REVIEW_BRANCH_CHILDREN: &[HelpNode] = &[
    node(
        "disable",
        &["selector [str]"],
        &[],
        "Disable a branch and kick off the rebase.",
        false,
        false,
        &[],
    ),
    node(
        "enable",
        &["selector [str]"],
        &[],
        "Enable a branch and kick off the rebase.",
        false,
        false,
        &[],
    ),
    node(
        "terminal",
        &["selector [str]"],
        &["--mode [open|readonly]"],
        "Print the command to resume a branch's conflict-resolver conversation locally.",
        false,
        false,
        &[],
    ),
];

const REVIEW_CHECKS_CHILDREN: &[HelpNode] = &[
    node(
        "list",
        &["selector [str]"],
        &[],
        "List the manual checks.",
        false,
        true, // ("review", "checks", "list")
        &[],
    ),
    node(
        "run",
        &["selector [str]"],
        &["--all", "--index [integer...]", "--input [name=value...]"],
        "Print the command(s) + cwd to run one/some/all manual checks yourself.",
        false,
        true, // ("review", "checks", "run") -- only ever prints, never executes
        &[],
    ),
    node(
        "terminal",
        &["selector [str]"],
        &["--mode [open|readonly]"],
        "Print the command to resume the manual-checks-generation agent conversation locally.",
        false,
        false,
        &[],
    ),
];

const REVIEW_ACTION_CHILDREN: &[HelpNode] = &[
    node(
        "list",
        &["selector [str]"],
        &[],
        "List the action hints.",
        false,
        true, // ("review", "action", "list")
        &[],
    ),
    node(
        "run",
        &["selector [str]"],
        &["--index [integer]", "--input [name=value...]"],
        "Print the command + cwd for a command-kind action hint.",
        false,
        true, // ("review", "action", "run") -- only ever prints, never executes
        &[],
    ),
];

const REVIEW_CHAT_CHILDREN: &[HelpNode] = &[
    node(
        "fork",
        &["selector [str]", "text [str]"],
        &["--seq [integer]"],
        "Fork the thread at a message, replacing it with new text.",
        false,
        false,
        &[],
    ),
    node(
        "send",
        &["selector [str]", "text [str]"],
        &[],
        "Post a message.",
        false,
        false,
        &[],
    ),
    node(
        "show",
        &["selector [str]"],
        &[],
        "Show the thread.",
        false,
        true, // ("review", "chat", "show")
        &[],
    ),
];

const REVIEW_CHILDREN: &[HelpNode] = &[
    node(
        "action",
        &[],
        &[],
        "User-declared [[review.action]] test/action hints.",
        false,
        false,
        REVIEW_ACTION_CHILDREN,
    ),
    node(
        "add-branch",
        &["selector [str]", "branch [str]"],
        &[],
        "Add a branch to a review.",
        false,
        false,
        &[],
    ),
    node(
        "approve",
        &["selector [str]"],
        &[],
        "Approve a review that is in_review.",
        false,
        false,
        &[],
    ),
    node(
        "base",
        &[],
        &[],
        "Inspect/change a review's base branch.",
        false,
        false,
        REVIEW_BASE_CHILDREN,
    ),
    node(
        "branch",
        &[],
        &[],
        "Enable/disable one review branch.",
        false,
        false,
        REVIEW_BRANCH_CHILDREN,
    ),
    node(
        "build-env",
        &["selector [str]"],
        &[
            "--clear [key...]",
            "--set [key=value...]",
            "--unset [key...]",
        ],
        "Set/unset/clear this review's build/check-gate step environment overrides.",
        false,
        false,
        &[],
    ),
    node(
        "cancel",
        &["selector [str]"],
        &[],
        "Cancel a review.",
        false,
        false,
        &[],
    ),
    node(
        "chat",
        &[],
        &[],
        "The review's global feedback thread.",
        false,
        false,
        REVIEW_CHAT_CHILDREN,
    ),
    node(
        "checks",
        &[],
        &[],
        "LLM-synthesized manual review-verification commands.",
        false,
        false,
        REVIEW_CHECKS_CHILDREN,
    ),
    node(
        "create",
        &["name [str]", "base_branch [str]", "git_root [str]"],
        &[
            "--checks [list]",
            "--review-type [label]",
            "--skip-auto-build",
            "--skip-worktree-checks",
            "--skip-worktrees",
        ],
        "Create a new review.",
        false,
        false,
        &[],
    ),
    node(
        "delete",
        &["selector [str]"],
        &["--yes"],
        "Delete a review and its worktrees.",
        false,
        false,
        &[],
    ),
    node(
        "dismiss-reenable",
        &["selector [str]"],
        &[],
        "Dismiss the 're-enable' notification for a branch.",
        false,
        false,
        &[],
    ),
    node(
        "feedback",
        &["selector [str]", "text [str]"],
        &[],
        "Post feedback on one branch, triggering a resolver re-attempt.",
        false,
        false,
        &[],
    ),
    node(
        "force-start",
        &["selector [str]"],
        &[],
        "Disable not-yet-done branches and merge immediately (only while collecting).",
        false,
        false,
        &[],
    ),
    node(
        "list",
        &[],
        &["--pr-ready", "--status [statuses]"],
        "List reviews.",
        false,
        true, // ("review", "list")
        &[],
    ),
    node(
        "logs",
        &["selector [str]"],
        &[],
        "Show a review's state-transition audit log.",
        false,
        true, // ("review", "logs")
        &[],
    ),
    node(
        "manual-checks-env",
        &["selector [str]"],
        &[
            "--clear [key...]",
            "--set [key=value...]",
            "--unset [key...]",
        ],
        "Set/unset/clear this review's manual-checks step environment overrides.",
        false,
        false,
        &[],
    ),
    node(
        "merge",
        &["selector [str]"],
        &[],
        "Start (or continue) the stacked rebase.",
        false,
        false,
        &[],
    ),
    node(
        "move-branch",
        &["selector [str]", "to_review [str]"],
        &[],
        "Move a branch to another review, then rebuild both.",
        false,
        false,
        &[],
    ),
    node(
        "pr",
        &[],
        &[],
        "Submit/query pull requests for a review.",
        false,
        false,
        REVIEW_PR_CHILDREN,
    ),
    node(
        "rename",
        &["selector [str]", "name [str]"],
        &[],
        "Rename a review.",
        false,
        false,
        &[],
    ),
    node(
        "reorder",
        &["selector [str]", "order [str]"],
        &["--disable [names]", "--enable [names]"],
        "Set the branch order and kick off the rebase.",
        false,
        false,
        &[],
    ),
    node(
        "restart-merge",
        &["selector [str]"],
        &[],
        "Cancel an in-progress rebase and start a fresh one.",
        false,
        false,
        &[],
    ),
    node(
        "settings",
        &["selector [str]"],
        &[
            "--auto-pr-feedback/--no-auto-pr-feedback",
            "--base-branch [branch]",
            "--resolver-agent [name]",
            "--resolver-model [name]",
            "--skip-auto-build/--no-skip-auto-build",
            "--skip-auto-clean/--no-skip-auto-clean",
            "--skip-worktree-checks/--no-skip-worktree-checks",
            "--skip-worktrees/--no-skip-worktrees",
            "--proof-scope [each_branch|final_branch|nothing]",
        ],
        "Update per-review opt-out settings.",
        false,
        false,
        &[],
    ),
    node(
        "show",
        &["selector [str]"],
        &[],
        "Show a single review's detail.",
        false,
        true, // ("review", "show")
        &[],
    ),
    node(
        "squash",
        &["selector [str]", "project [str]"],
        &["--off", "--on"],
        "Enable/disable squashing one git project's task branches to a single commit each in \
the review worktree.",
        false,
        false,
        &[],
    ),
    node(
        "status",
        &["selector [str]"],
        &[],
        "Per-branch readiness + a summary verdict ('is this review ready?').",
        false,
        true, // ("review", "status")
        &[],
    ),
    node(
        "worktrees",
        &["selector [str]"],
        &[],
        "The worktrees/branches this review consumes.",
        false,
        true, // ("review", "worktrees")
        &[],
    ),
];

// ---- other top-level groups' children -------------------------------------

const AGENT_CHILDREN: &[HelpNode] = &[node(
    "list",
    &[],
    &[],
    "List supported agent backends and the models each is allowed to run.",
    false,
    true, // ("agent", "list")
    &[],
)];

const CHECK_CHILDREN: &[HelpNode] = &[node(
    "health",
    &[],
    &["--enable-developer-checks"],
    "Check the local ralphus setup (daemon, git, runner, ollama).",
    true, // ("check", "health") -- multi-step subprocess/filesystem work
    true, // ("check", "health")
    &[],
)];

const INITIALIZE_CHILDREN: &[HelpNode] = &[node(
    "git",
    &[],
    &["--path [path]"],
    "Enable git rerere in a repo so review rebases replay conflict resolutions.",
    false,
    false,
    &[],
)];

const MACHINE_CHILDREN: &[HelpNode] = &[
    node(
        "cleanup",
        &["machine [str]"],
        &[],
        "Tear down one provisioned workspace on a machine provider (RAL-201).",
        false,
        false,
        &[],
    ),
    node(
        "get",
        &["scheme [str]"],
        &[],
        "Show one registered machine provider by exact scheme.",
        false,
        true, // ("machine", "get")
        &[],
    ),
    node(
        "list",
        &[],
        &[],
        "List every registered machine provider, plus built-in schemes.",
        false,
        true, // ("machine", "list")
        &[],
    ),
    node(
        "register",
        &[],
        &[
            "--arg [value...]",
            "--channel",
            "--description [text]",
            "--program [path]",
            "--scheme [name]",
        ],
        "Register a provider program a task's 'machine' field can reference.",
        false,
        false,
        &[],
    ),
    node(
        "remove",
        &["scheme [str]"],
        &[],
        "Remove a registered machine provider.",
        false,
        false,
        &[],
    ),
];

const PROJECT_CHILDREN: &[HelpNode] = &[
    node(
        "get",
        &["name [str]"],
        &[],
        "Show one registered project's details by exact name.",
        false,
        true, // ("project", "get")
        &[],
    ),
    node(
        "git",
        &[],
        &["--description [text]", "--name [name]", "--path [path]"],
        "Register a git repository as a project the daemon can resolve placeholder cell \
cwds against.",
        false,
        false,
        &[],
    ),
    node(
        "list",
        &[],
        &["--short"],
        "List every project registered with the daemon.",
        false,
        true, // ("project", "list")
        &[],
    ),
];

const MAILBOX_CHILDREN: &[HelpNode] = &[node(
    "check",
    &[],
    &["--priority [urgent|high|normal]"],
    "Drain unread escalation mailbox messages and print them (RAL-241).",
    false,
    false, // drains (marks read) as a side effect -- not read-only
    &[],
)];

const QUEUE_CHILDREN: &[HelpNode] = &[
    node(
        "list",
        &[],
        &["--all"],
        "List queued work items (ready-to-run by default).",
        false,
        true, // ("queue", "list")
        &[],
    ),
    node(
        "reorder",
        &["paths [str...]"],
        &[],
        "Set the queue order to the given item paths (dependency-repaired).",
        false,
        false,
        &[],
    ),
    node(
        "set-position",
        &["paths [str...]"],
        &["--relative", "--to [integer]"],
        "Move item(s) to an absolute index or a relative offset.",
        false,
        false,
        &[],
    ),
    node(
        "set-status",
        &["path [str]", "state [str]"],
        &[],
        "Set a squad/task/cell/proof status (e.g. ignored) by item path or squad id.",
        false,
        false,
        &[],
    ),
];

const SQUAD_CHILDREN: &[HelpNode] = &[
    node(
        "activate",
        &["squad_id [str]"],
        &[],
        "Promote a held (queued) squad to pending.",
        false,
        false,
        &[],
    ),
    node(
        "cancel",
        &["squad_id [str]"],
        &[],
        "Cancel a squad.",
        false,
        false,
        &[],
    ),
    node(
        "delete",
        &["squad_id [str]"],
        &["--yes"],
        "Permanently delete a squad.",
        false,
        false,
        &[],
    ),
    node(
        "edit",
        &["squad_id [str]"],
        &["--label [text]"],
        "Edit a squad's fields.",
        false,
        false,
        &[],
    ),
    node(
        "list",
        &[],
        &[
            "--name [substring]",
            "--sort [date|name]",
            "--status [states]",
        ],
        "List squads.",
        false,
        true, // ("squad", "list")
        &[],
    ),
    node(
        "logs",
        &["squad_id [str]"],
        &[],
        "Show a squad's state-transition audit log.",
        false,
        true, // ("squad", "logs")
        &[],
    ),
    node(
        "rename",
        &["squad_id [str]", "label [str]"],
        &[],
        "Rename a squad's label.",
        false,
        false,
        &[],
    ),
    node(
        "restart",
        &["squad_id [str]"],
        &[],
        "Restart a whole squad, dirtying every squad that depends on it.",
        false,
        false,
        &[],
    ),
    node(
        "retry",
        &["squad_id [str]"],
        &[],
        "Re-run with the same parameters (reset to pending).",
        false,
        false,
        &[],
    ),
    node(
        "set-status",
        &["squad_id [str]", "state [str]"],
        &[],
        "Manually override a squad's status.",
        false,
        false,
        &[],
    ),
    node(
        "show",
        &["squad_id [str]"],
        &[],
        "Show a single squad's detail.",
        false,
        true, // ("squad", "show")
        &[],
    ),
    node(
        "timeline",
        &["squad_id [str]"],
        &["--write [path]"],
        "Generate the merged, chronological uber-log-viewer timeline for a squad (RAL-155).",
        false,
        false,
        &[],
    ),
];

const CELL_CHILDREN: &[HelpNode] = &[
    node(
        "edit",
        &["selector [str]"],
        &[
            "--agent [name]",
            "--command [cmd]",
            "--cwd [path]",
            "--model [name]",
            "--prompt [text]",
        ],
        "Edit a cell's fields.",
        false,
        false,
        &[],
    ),
    node(
        "restart",
        &["selector [str]"],
        &[],
        "Restart a cell (and its downstream), dirtying dependent squads.",
        false,
        false,
        &[],
    ),
    node(
        "restart-proof",
        &["selector [str]"],
        &["--from [index]"],
        "Restart a cell's proof steps from an index onwards.",
        false,
        false,
        &[],
    ),
    node(
        "reviews",
        &["selector [str]"],
        &[],
        "The reviews this cell's branch participates in.",
        false,
        true, // ("cell", "reviews")
        &[],
    ),
    node(
        "set-status",
        &["selector [str]", "state [str]"],
        &[],
        "Manually override a cell's status.",
        false,
        false,
        &[],
    ),
    node(
        "show",
        &["selector [str]"],
        &[],
        "Show a single cell's detail.",
        false,
        true, // ("cell", "show")
        &[],
    ),
    node(
        "terminal",
        &["selector [str]"],
        &["--mode [open|readonly]"],
        "Print the command to resume a cell's conversation locally.",
        false,
        true, // ("cell", "terminal")
        &[],
    ),
    node(
        "worktree",
        &["selector [str]"],
        &[],
        "Show the worktree/project a cell is using.",
        false,
        true, // ("cell", "worktree")
        &[],
    ),
];

const SHOW_CHILDREN: &[HelpNode] = &[node(
    "help-map",
    &[],
    &[],
    "Print the full CLI command surface as an alphabetized, indented tree (for onboarding an \
AI agent).",
    false,
    true, // ("show", "help-map")
    &[],
)];

const TASK_CHILDREN: &[HelpNode] = &[
    node(
        "edit",
        &["selector [str]"],
        &["--name [name]", "--project [name]"],
        "Edit a task node's name/project.",
        false,
        false,
        &[],
    ),
    node(
        "restart-proof",
        &["selector [str]"],
        &["--from [index]"],
        "Restart a task's proof steps from an index onwards.",
        false,
        false,
        &[],
    ),
    node(
        "set-status",
        &["selector [str]", "state [str]"],
        &[],
        "Manually override a task's status.",
        false,
        false,
        &[],
    ),
    node(
        "show",
        &["selector [str]"],
        &[],
        "Show a single task node's detail.",
        false,
        true, // ("task", "show")
        &[],
    ),
    // No "show-tutor" child here -- this crate hoists that behavior to the
    // top-level `tutor` command instead (see module doc comment above and
    // `commands/task.rs`'s own doc comment).
];

const PROOF_CHILDREN: &[HelpNode] = &[
    node(
        "restart",
        &["selector [str]"],
        &[],
        "Restart this proof step (and any later ones in its scope).",
        false,
        false,
        &[],
    ),
    node(
        "set-status",
        &["selector [str]", "state [str]"],
        &[],
        "Manually override a proof step's status.",
        false,
        false,
        &[],
    ),
    node(
        "show",
        &["selector [str]"],
        &[],
        "Show a single proof step's detail.",
        false,
        true, // ("proof", "show")
        &[],
    ),
];

// ---- the whole tree --------------------------------------------------------

/// The full command tree, rooted at `ralphus`. `author`/`quick-start` are
/// excluded entirely (mirrors Python's `_HIDDEN_COMMANDS`); see module doc
/// comment for the handful of other disclosed structural differences from
/// `cli/src/ralphus/helpmap.py`'s Python tree.
pub const ROOT: HelpNode = node(
    "ralphus",
    &[],
    &["--daemon-url [url]", "--json", "--version"],
    "Submit and manage autonomous agent tasks against the ralphus daemon.",
    false,
    false,
    &[
        node(
            "agent",
            &[],
            &[],
            "Inspect agent backends ralphus can run.",
            false,
            false,
            AGENT_CHILDREN,
        ),
        node(
            "cartographer",
            &[],
            &[
                "--ascending",
                "--cell [str]",
                "--entity [str]",
                "--for [str]",
                "--guardian [str]",
                "--level [str]",
                "--limit [integer]",
                "--offset [integer]",
                "--q [str]",
                "--scope [str]",
                "--source [str]",
                "--squad [str]",
                "--task [str]",
            ],
            "Query the structured Cartographer event log (RAL-98/RAL-155).",
            false,
            false,
            &[],
        ),
        node(
            "check",
            &[],
            &[],
            "System and environment checks.",
            false,
            false,
            CHECK_CHILDREN,
        ),
        node(
            "clear",
            &[],
            &["--all", "--keep-temporary", "--status [states]", "--yes"],
            "Delete tasks and reviews from the daemon.",
            true, // ("clear",) -- destructive
            false,
            &[],
        ),
        node(
            "completion",
            &[],
            &[],
            "Print a shell tab-completion script. (Rust port: not yet implemented -- prints a \
placeholder message; Python's `shell` argument is not read.)",
            false,
            true, // ("completion",)
            &[],
        ),
        node(
            "configuration",
            &[],
            &[],
            "Show sourced .ralphus.toml files and resolved values. (Python's separate \
`configuration show` subcommand is flattened into this bare command in the Rust port; \
--no-local is not yet ported.)",
            false,
            true, // matches Python's ("configuration", "show")
            &[],
        ),
        node(
            "get",
            &["selector [str]", "field [str, optional]"],
            &[],
            "Query one field from any entity's JSON view (jq-lite).",
            false,
            true, // ("get",)
            &[],
        ),
        node(
            "graph",
            &["squad_id [str, optional]"],
            &["--all", "--dot"],
            "Render the task-order dependency graph. (Rust port simplifies Python's \
--global/--format ascii|dot choice to plain --dot/--all boolean flags.)",
            false,
            true, // ("graph",)
            &[],
        ),
        node(
            "history",
            &["selector [str]"],
            &[],
            "Show a cell/proof step's tmux history (one-shot snapshot; Python's --live \
tailing and --wait-until-valid are not yet ported).",
            false,
            true, // ("history",)
            &[],
        ),
        node(
            "initialize",
            &[],
            &[],
            "One-time local setup helpers for a repository.",
            false,
            false,
            INITIALIZE_CHILDREN,
        ),
        node(
            "listen",
            &["selector [str]"],
            &["--timeout [seconds]", "--until [status]"],
            "Block until a squad/task/cell/proof/review/review-worktree reaches a status.",
            false,
            true, // ("listen",)
            &[],
        ),
        node(
            "license",
            &[],
            &[],
            "Print the embedded LICENSE text decoded from the binary's obfuscated copy.",
            false,
            true, // ("license",)
            &[],
        ),
        node(
            "machine",
            &[],
            &[],
            "Register and inspect machine providers remote work runs on.",
            false,
            false,
            MACHINE_CHILDREN,
        ),
        node(
            "mailbox",
            &[],
            &[],
            "Drain the escalation mailbox (RAL-241): failed/stalled work the daemon flagged for attention.",
            false,
            false,
            MAILBOX_CHILDREN,
        ),
        node(
            "project",
            &[],
            &[],
            "Register and inspect projects known to the daemon.",
            false,
            false,
            PROJECT_CHILDREN,
        ),
        node(
            "queue",
            &[],
            &[],
            "Inspect and reorder the squad queue by priority.",
            false,
            false,
            QUEUE_CHILDREN,
        ),
        node(
            "resources",
            &[],
            &[],
            "Show per-task resource usage (CPU/RAM/GPU).",
            false,
            true, // ("resources",)
            &[],
        ),
        node(
            "retry",
            &["squad_id [str]"],
            &[],
            "Re-run a squad from scratch (reset to pending). (Rust port: squad-level only; \
Python's per-selector --environment/--env-file overrides are not yet ported.)",
            false,
            false,
            &[],
        ),
        node(
            "review",
            &[],
            &[],
            "Inspect and act on reviews (guardians).",
            true, // ("review",) -- whole group tagged, not each child individually
            false,
            REVIEW_CHILDREN,
        ),
        node(
            "squad",
            &[],
            &[],
            "Inspect and act on squads.",
            false,
            false,
            SQUAD_CHILDREN,
        ),
        node(
            "cell",
            &[],
            &[],
            "Inspect and act on cells.",
            false,
            false,
            CELL_CHILDREN,
        ),
        node(
            "show",
            &[],
            &[],
            "Print machine-readable views of ralphus itself.",
            false,
            false,
            SHOW_CHILDREN,
        ),
        node(
            "status",
            &["squad_id [str, optional]"],
            &["--concurrency"],
            "Show squad status from the daemon.",
            false,
            true, // ("status",)
            &[],
        ),
        node(
            "submit",
            &["file [str...]"],
            &[
                "--activate",
                "--hold",
                "--label [text]",
                "--no-validate",
                "--wait",
            ],
            "Submit one or more task TOML files to the daemon.",
            true, // ("submit",) -- slow/blocking, esp. with --wait
            false,
            &[],
        ),
        node(
            "task",
            &[],
            &[],
            "Task-authoring helpers and task-node inspection.",
            false,
            false,
            TASK_CHILDREN,
        ),
        node(
            "tutor",
            &[],
            &[],
            "Print the Task TOML schema reference and worked examples. (Rust port hoists \
Python's `task show-tutor` to this top-level command.)",
            false,
            true, // matches Python's ("task", "show-tutor")
            &[],
        ),
        node(
            "validate",
            &["file [path...]"],
            &[],
            "Validate one or more task TOML files.",
            false,
            true, // ("validate",)
            &[],
        ),
        node(
            "proof",
            &[],
            &[],
            "Inspect and act on proof steps.",
            false,
            false,
            PROOF_CHILDREN,
        ),
    ],
);

// ---- rendering --------------------------------------------------------------

/// Renders `node` (one line) then every child recursively, one level deeper
/// -- ports `helpmap.py::_render`'s exact formatting: positional chips
/// (declared order) then sorted optional chips, an optional `(subagent)`
/// trailing tag, an optional `(read-only-safe) ` leading prefix, and finally
/// a `{description}` suffix. Children are alphabetized by name.
fn render(n: &HelpNode, depth: usize, out: &mut String) {
    let indent = "    ".repeat(depth);
    let mut chips: Vec<&str> = n.positionals.to_vec();
    let mut opts: Vec<&str> = n.options.to_vec();
    opts.sort_unstable();
    chips.extend(opts);
    let head = if chips.is_empty() {
        n.name.to_string()
    } else {
        format!("{} {}", n.name, chips.join(" "))
    };
    let marker = if n.subagent { " (subagent)" } else { "" };
    let prefix = if n.read_only_safe {
        "(read-only-safe) "
    } else {
        ""
    };
    out.push_str(&format!(
        "{indent}- {prefix}{head}{marker}  {{{}}}\n",
        n.description
    ));
    let mut children: Vec<&HelpNode> = n.children.iter().collect();
    children.sort_by_key(|c| c.name);
    for child in children {
        render(child, depth + 1, out);
    }
}

fn find_node<'a>(node: &'a HelpNode, path: &[&str]) -> Option<&'a HelpNode> {
    match path.split_first() {
        None => Some(node),
        Some((head, tail)) => node
            .children
            .iter()
            .find(|child| child.name == *head)
            .and_then(|child| find_node(child, tail)),
    }
}

fn signature(n: &HelpNode) -> String {
    let mut chips: Vec<&str> = n.positionals.to_vec();
    let mut opts: Vec<&str> = n.options.to_vec();
    opts.sort_unstable();
    chips.extend(opts);
    if chips.is_empty() {
        n.name.to_string()
    } else {
        format!("{} {}", n.name, chips.join(" "))
    }
}

fn command_path(path: &[&str]) -> String {
    if path.is_empty() {
        "ralphus".to_string()
    } else {
        format!("ralphus {}", path.join(" "))
    }
}

#[must_use]
pub fn command_help(path: &[&str]) -> Option<String> {
    let node = find_node(&ROOT, path)?;
    let full = command_path(path);
    let mut out = String::new();
    out.push_str(&format!("{full} -- {}\n", node.description));
    out.push_str("USAGE:\n    ");
    out.push_str(&full);
    if !node.positionals.is_empty() {
        out.push(' ');
        out.push_str(&node.positionals.join(" "));
    }
    if !node.options.is_empty() {
        out.push_str(" [OPTIONS]");
    }
    if !node.children.is_empty() {
        out.push_str(" <SUBCOMMAND> [ARGS...]");
    }
    out.push('\n');
    if !node.options.is_empty() {
        let mut opts: Vec<&str> = node.options.to_vec();
        opts.sort_unstable();
        out.push_str("\nOPTIONS:\n");
        for opt in opts {
            out.push_str(&format!("    {opt}\n"));
        }
    }
    if !node.children.is_empty() {
        let mut children: Vec<&HelpNode> = node.children.iter().collect();
        children.sort_by_key(|child| child.name);
        out.push_str("\nSUBCOMMANDS:\n");
        for child in children {
            out.push_str(&format!(
                "    {:<32} {}\n",
                signature(child),
                child.description
            ));
        }
    }
    Some(out.trim_end().to_string())
}

/// The full alphabetized, indented help-map tree, as one string -- ports
/// `helpmap.py::generate()`.
#[must_use]
pub fn generate() -> String {
    let mut out = String::new();
    render(&ROOT, 0, &mut out);
    // `render` appends a trailing "\n" after every line (including the
    // last); Python's `"\n".join(lines)` has no trailing newline, so trim it
    // to match exactly.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// The six guidance notes, blank-line separated, followed by [`generate()`]'s
/// tree -- ports `helpmap.py::main()`'s exact print sequence. This is what
/// `ralphus show help-map` prints.
#[must_use]
pub fn full_output() -> String {
    format!(
        "{SUBAGENT_NOTE}\n\n{READ_ONLY_NOTE}\n\n{PROJECT_LOOKUP_NOTE}\n\n{SUBMIT_VALIDATE_NOTE}\n\n\
{SUBMIT_REVIEW_NOTE}\n\n{JSON_NOTE}\n\n{}",
        generate()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CHILD: HelpNode = node(
        "alpha",
        &["pos1"],
        &["--zeta", "--beta [val]"],
        "does alpha things",
        false,
        true,
        &[],
    );

    const SAMPLE_ROOT: HelpNode = node(
        "root",
        &[],
        &["--json"],
        "root desc",
        true,
        false,
        &[SAMPLE_CHILD],
    );

    #[test]
    fn renders_indentation_and_child_nesting() {
        let mut out = String::new();
        render(&SAMPLE_ROOT, 0, &mut out);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("    - "));
    }

    #[test]
    fn sorts_option_chips_alphabetically_but_keeps_positional_order_first() {
        let mut out = String::new();
        render(&SAMPLE_CHILD, 0, &mut out);
        // positional first, then options alphabetized (--beta before --zeta)
        assert!(out.contains("alpha pos1 --beta [val] --zeta"));
    }

    #[test]
    fn read_only_safe_prefix_comes_before_name_subagent_after_chips() {
        let mut out = String::new();
        render(&SAMPLE_CHILD, 0, &mut out);
        assert!(out.starts_with("- (read-only-safe) alpha "));
        let mut out2 = String::new();
        render(&SAMPLE_ROOT, 0, &mut out2);
        assert!(out2.starts_with("- root --json (subagent)  {root desc}"));
    }

    #[test]
    fn description_is_wrapped_in_braces() {
        let mut out = String::new();
        render(&SAMPLE_CHILD, 0, &mut out);
        assert!(out.contains("{does alpha things}"));
    }

    #[test]
    fn generate_has_no_trailing_newline() {
        let text = generate();
        assert!(!text.ends_with('\n'));
        assert!(text.starts_with("- ralphus"));
    }

    #[test]
    fn full_output_prints_all_six_notes_before_the_tree() {
        let text = full_output();
        assert!(text.starts_with(SUBAGENT_NOTE));
        for note in [
            SUBAGENT_NOTE,
            READ_ONLY_NOTE,
            PROJECT_LOOKUP_NOTE,
            SUBMIT_VALIDATE_NOTE,
            SUBMIT_REVIEW_NOTE,
            JSON_NOTE,
        ] {
            assert!(text.contains(note));
        }
        assert!(text.contains("- ralphus"));
    }

    #[test]
    fn real_tree_covers_top_level_groups_and_excludes_hidden_commands() {
        let names: Vec<&str> = ROOT.children.iter().map(|c| c.name).collect();
        assert!(names.contains(&"review"));
        assert!(names.contains(&"squad"));
        assert!(names.contains(&"task"));
        assert!(names.contains(&"cartographer"));
        assert!(names.contains(&"completion"));
        assert!(!names.contains(&"author"));
        assert!(!names.contains(&"quick-start"));
    }

    #[test]
    fn positionals_carry_type_annotations_not_bare_names() {
        fn walk<'a>(n: &'a HelpNode, out: &mut Vec<&'a str>) {
            out.extend(n.positionals.iter().copied());
            for child in n.children {
                walk(child, out);
            }
        }
        let mut positionals = Vec::new();
        walk(&ROOT, &mut positionals);
        assert!(!positionals.is_empty());
        for p in positionals {
            assert!(
                p.contains('['),
                "positional chip '{p}' is missing a type annotation"
            );
        }
    }

    #[test]
    fn no_flag_uses_the_bare_n_placeholder() {
        fn walk<'a>(n: &'a HelpNode, out: &mut Vec<&'a str>) {
            out.extend(n.options.iter().copied());
            for child in n.children {
                walk(child, out);
            }
        }
        let mut options = Vec::new();
        walk(&ROOT, &mut options);
        for o in options {
            assert!(
                !o.contains("[n]") && !o.contains("[n..."),
                "flag '{o}' uses the bare 'n' placeholder instead of [integer]/[float]"
            );
        }
    }

    #[test]
    fn real_tree_has_non_trivial_node_count() {
        fn count(n: &HelpNode) -> usize {
            1 + n.children.iter().map(count).sum::<usize>()
        }
        // ~105 leaf commands per the port's own scale estimate; require a
        // generously loose lower bound so this isn't brittle against small
        // future additions/removals.
        assert!(count(&ROOT) > 80, "node count: {}", count(&ROOT));
    }

    #[test]
    fn review_top_level_is_tagged_subagent_but_children_are_not() {
        let review = ROOT.children.iter().find(|c| c.name == "review").unwrap();
        assert!(review.subagent);
        for child in review.children {
            assert!(
                !child.subagent,
                "review child '{}' should not itself be tagged subagent",
                child.name
            );
        }
    }

    #[test]
    fn check_health_is_tagged_both_subagent_and_read_only_safe() {
        let check = ROOT.children.iter().find(|c| c.name == "check").unwrap();
        let health = check.children.iter().find(|c| c.name == "health").unwrap();
        assert!(health.subagent);
        assert!(health.read_only_safe);
    }

    #[test]
    fn command_help_lists_root_subcommands_alphabetically() {
        let text = command_help(&[]).expect("root help");
        assert!(text.contains("USAGE:\n    ralphus [OPTIONS] <SUBCOMMAND> [ARGS...]"));
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("    "))
            .collect();
        let queue = lines
            .iter()
            .position(|line| line.trim_start().starts_with("queue"))
            .expect("queue present");
        let review = lines
            .iter()
            .position(|line| line.trim_start().starts_with("review"))
            .expect("review present");
        let show = lines
            .iter()
            .position(|line| line.trim_start().starts_with("show"))
            .expect("show present");
        assert!(queue < review);
        assert!(review < show);
    }

    #[test]
    fn command_help_for_nested_group_lists_immediate_children() {
        let text = command_help(&["review", "pr"]).expect("review pr help");
        assert!(text.contains("ralphus review pr --"));
        assert!(text.contains("SUBCOMMANDS:"));
        assert!(text.contains("comments"));
        assert!(text.contains("pull-feedback"));
        assert!(text.contains("submit"));
    }
}
