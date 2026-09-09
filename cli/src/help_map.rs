//! `ralphus show help-map`, ported from `cli/src/ralphus/helpmap.py` (RAL-110).
//!
//! This crate has no `argparse` tree to introspect (see
//! `cli/src/flags.rs`'s hand-rolled `Scanner`), so the command surface is
//! hand-encoded as a `const`/`static` tree ([`ROOT`]) built directly from two
//! sources:
//!
//! - **Which commands/flags exist** comes from this crate's own
//!   `cli/src/commands/*.rs` (`parse()` functions and `Command`/
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
//! `quick-start` is excluded from the AI-oriented generated map, mirroring
//! Python's `_HIDDEN_COMMANDS`, but is present in the user-facing command
//! registry below. The registry gates dispatch, so a command cannot become
//! callable without first acquiring a help definition.

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
before the command name, never as one of its own flags) describe a property of the command \
itself: it performs no mutation, so it is safe to run even under a read-only restriction. The \
tag is not a permission grant -- in a normal session you may call ANY command below, tagged or \
not. It becomes a restriction only when THIS session was itself launched with `--read-only` \
(`quick-start manager --read-only` / `quick-start reviewer --read-only`; see `ralphus \
quick-start manager --help` / `ralphus quick-start reviewer --help`), in which case the tree \
below has already been filtered down to only `(read-only-safe)` commands, and calling anything \
outside it is off-limits. Everything below is assumed mutating unless tagged.";

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

pub const SUBMIT_RETRY_NOTE: &str = "If `submit` (or any other mutating command) errors out with \
a client-side network/timeout error -- not a real error message returned by the daemon -- that \
does NOT mean the request was rejected: the daemon may have already accepted it and started work \
before the response was lost. Never blindly retry in that situation. First run `ralphus status` \
or `ralphus queue list --all` and look for a squad/entity matching what you just tried to create \
that's already `pending`/`running`. Only resubmit once you've confirmed no such entity exists -- \
otherwise you create a genuine duplicate (e.g. two copies of every task in a batch running in \
parallel) that then has to be manually cancelled.";

pub const JSON_NOTE: &str = "`--json` (emit raw daemon JSON instead of human-readable text) \
works for every command below, even though it's only listed on the root `ralphus` line of the \
tree -- it's a global flag, not a per-command one. Unlike other global flags, it works BOTH \
before and after the subcommand: `ralphus --json status` and `ralphus status --json` are \
equivalent.";

/// Syntax and examples for arguments displayed as `[uri]`.
pub const URI_ARGUMENT_NOTE: &str = "`[uri]` arguments use the EntityUri grammar: `squad:<squad_id>` (e.g. `squad:squad-1`); `task:<squad_id>:<task_idx>` (e.g. `task:squad-1:2`); `cell:<squad_id>:<task_idx>:<cell_idx>` (e.g. `cell:squad-1:2:0`); `proof:<squad_id>:<task_idx>:<proof_scope>:<cell_idx>:<proof_idx>` (e.g. `proof:squad-1:2:cell:0:1`; use `task` and `-1` for a task-scoped proof); and `guardian:<guardian_id>` (e.g. `guardian:g-1`) for a review. `selector` also accepts its documented name and index forms in addition to these URIs.";

// ---- review subgroups (defined separately to keep REVIEW_CHILDREN readable) --

const REVIEW_UPSTREAM_CHILDREN: &[HelpNode] = &[
    node(
        "list",
        &["selector [str]"],
        &[],
        "List candidate upstream branches.",
        false,
        true, // ("review", "upstream", "list")
        &[],
    ),
    node(
        "set",
        &["selector [str]", "branch [str]"],
        &[],
        "Change the upstream branch.",
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
        "pull-from-pr",
        &["pr_id [str]"],
        &[],
        "Pull a reviewer's commits pushed directly to the PR branch back into the owning review \
worktree, resolving conflicts and restacking downstream branches (RAL-190).",
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
            "--allow-unlinked-fork",
            "--combined",
            "--description [text]",
            "--position [integer]",
            "--title [text]",
            "--use-worktree-branch-name",
        ],
        "Submit a PR/MR for one stacked branch or the combined worktree. --allow-unlinked-fork \
(RAL-338) downgrades a definite \"no forge relationship\" fork pre-flight result from a hard \
error to a logged warning; ignored for a project with no registered fork.",
        false,
        false,
        &[],
    ),
    node(
        "unlink",
        &["selector [str]"],
        &[],
        "Bulk-drop every currently open PR row for a review and clear its registered forge PR \
stack number, so a later submission starts a fresh stack instead of appending to one whose PRs \
were just unlinked (RAL-317).",
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
        "upstream",
        &[],
        &[],
        "Inspect/change a review's upstream branch.",
        false,
        false,
        REVIEW_UPSTREAM_CHILDREN,
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
        "reopen",
        &["selector [str]"],
        &[],
        "Reopen a cancelled review and immediately stage in whatever branches are already ready, without waiting for the rest.",
        false,
        false,
        &[],
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
        "env",
        &["selector [str]"],
        &["--scope [build|tests|manual-checks|worktree]"],
        "List a review surface's resolved environment variables, read-only (RAL-324): the auto-build step, the check gates, manual checks, or one branch's review worktree.",
        false,
        true, // ("review", "env")
        &[],
    ),
    node(
        "feedback",
        &["selector [str]", "text [str]"],
        &["--author [name]"],
        "Post feedback on one branch, triggering a resolver re-attempt. --author attributes the feedback to a different registered user than the one submitting it (RAL-379); defaults to the submitter when omitted.",
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
        "stop-merge",
        &["selector [str]"],
        &[],
        "Stop an in-progress rebase at the next checkpoint, leaving the review resumable instead of cancelled.",
        false,
        false,
        &[],
    ),
    node(
        "settings",
        &["selector [str]"],
        &[
            "--auto-pr-feedback/--no-auto-pr-feedback",
            "--auto-submit-pr-stack/--no-auto-submit-pr-stack",
            "--base-branch [branch]",
            "--match-pr-branch-name/--no-match-pr-branch-name",
            "--resolver-agent [name]",
            "--resolver-model [name]",
            "--separate-pr-branch/--no-separate-pr-branch",
            "--skip-auto-build/--no-skip-auto-build",
            "--skip-auto-clean/--no-skip-auto-clean",
            "--skip-base-updates/--no-skip-base-updates",
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
        "sync-pr",
        &["selector [str]"],
        &[],
        "Check the forge for a stack reorder made outside ralphus and apply it if found.",
        false,
        false,
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
    node(
        "worktree-retirements",
        &[],
        &["--state [scheduled|eligible|claimed|failed|deferred|opted_out|retired]"],
        "List review worktrees across every review by retirement state -- scheduled, \
         eligible, claimed, failed, deferred, opted_out, retired -- with failure context and \
         the eligible-at timestamp. Repeat --state to filter (no --state lists everything).",
        false,
        true, // ("review", "worktree-retirements")
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
    &["--enable-developer-checks", "--all-remotes", "--json"],
    "Check the local ralphus setup (daemon, git, runner, ollama). --all-remotes also checks every configured [machine.targets.*] entry (RAL-355 Phase 9).",
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

const TRIAGE_TYPE_CHILDREN: &[HelpNode] = &[
    node(
        "deregister",
        &["name [str]"],
        &[],
        "Remove a Triage type. The built-in \"unclassified\" type can never be deregistered.",
        false,
        false,
        &[],
    ),
    node(
        "get",
        &["name [str]"],
        &[],
        "Show one registered Triage type by exact name.",
        false,
        true, // ("triage", "type", "get")
        &[],
    ),
    node(
        "list",
        &[],
        &[],
        "List every registered Triage type, including the built-in \"unclassified\" type.",
        false,
        true, // ("triage", "type", "list")
        &[],
    ),
    node(
        "register",
        &["name [str]"],
        &["--description [text]", "--label [text]"],
        "Register (or update) a Triage type -- the categories the Arbiter classifies a Triage-opted-in cell into (RAL-318).",
        false,
        false,
        &[],
    ),
];

const TRIAGE_POOL_CHILDREN: &[HelpNode] = &[
    node(
        "list",
        &[],
        &[],
        "List every Triage pool key with pooled cells and/or a configured count threshold, plus its resolved project name (RAL-318).",
        false,
        true, // ("triage", "pool", "list")
        &[],
    ),
    node(
        "threshold",
        &["project [str]", "triage_type [str]"],
        &["--threshold [integer]", "--clear"],
        "Set (or --clear) the count threshold for a (project, triage_type) pool -- once it holds this many cells, it drains into a fresh review (RAL-318).",
        false,
        false,
        &[],
    ),
];

const TRIAGE_CHILDREN: &[HelpNode] = &[
    node(
        "pool",
        &[],
        &[],
        "Inspect and configure Triage auto-review pools (RAL-318).",
        false,
        false,
        TRIAGE_POOL_CHILDREN,
    ),
    node(
        "type",
        &[],
        &[],
        "Register and inspect Triage types (RAL-318).",
        false,
        false,
        TRIAGE_TYPE_CHILDREN,
    ),
];

const MACHINE_CHILDREN: &[HelpNode] = &[
    node(
        "cleanup",
        &["machine [str]"],
        &["--project [name]", "--branch [name]"],
        "Tear down one project's provisioned workspace on a machine provider -- the whole project directory, or just --branch's worktree (RAL-201, reshaped by RAL-355 Phase 2).",
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

// RAL-338: per-project, per-user fork registration -- see
// `crate::commands::project::ProjectForkCommand`.
const PROJECT_FORK_CHILDREN: &[HelpNode] = &[
    node(
        "add",
        &["project [str]"],
        &[
            "--owner [owner]",
            "--remote-name [name]",
            "--url [url]",
            "--user [name]",
        ],
        "Register a fork for a project, optionally scoped to one user (defaults to the \
project-wide fallback row when --user is omitted).",
        false,
        false,
        &[],
    ),
    node(
        "list",
        &["project [str, optional]"],
        &["--short", "--user [name]"],
        "List registered forks, optionally scoped to one project and/or filtered to one user.",
        false,
        true, // ("project", "fork", "list")
        &[],
    ),
    node(
        "set",
        &["project [str]"],
        &[
            "--owner [owner]",
            "--remote-name [name]",
            "--url [url]",
            "--user [name]",
        ],
        "Update fields on an existing fork registration (defaults to the project-wide \
fallback row when --user is omitted).",
        false,
        false,
        &[],
    ),
    node(
        "remove",
        &["project [str]"],
        &["--user [name]"],
        "Remove a fork registration (defaults to the project-wide fallback row when \
--user is omitted).",
        false,
        false,
        &[],
    ),
];

const PROJECT_CHILDREN: &[HelpNode] = &[
    node(
        "fork",
        &[],
        &[],
        "Manage per-project, per-user fork registrations for fork-based stacked PR routing.",
        false,
        false,
        PROJECT_FORK_CHILDREN,
    ),
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
        &[
            "--clear-url",
            "--description [text]",
            "--match-pr-branch-name/--no-match-pr-branch-name",
            "--name [name]",
            "--path [path]",
            "--url [url]",
        ],
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

const MAILBOX_CHILDREN: &[HelpNode] = &[
    node(
        "check",
        &[],
        &["--priority [urgent|high|normal]", "--category [name]"],
        "Drain unread escalation mailbox messages and print them (RAL-241). \
         --category restricts to one message category, e.g. \"review\" (RAL-375).",
        false,
        false, // drains (marks read) as a side effect -- not read-only
        &[],
    ),
    node(
        "personal",
        &[],
        &[
            "--priority [urgent|high|normal]",
            "--unread",
            "--user [name]",
        ],
        "List the acting user's personal mailbox messages, filtered through their watches \
         (RAL-320).",
        false,
        true, // read-only: lists messages, never marks them read.
        &[],
    ),
    node(
        "personal-drain",
        &[],
        &["--id [id...]", "--user [name]"],
        "Mark personal mailbox messages read; omit --id to drain every unread message (RAL-320).",
        false,
        false, // mutates read state.
        &[],
    ),
    node(
        "watch",
        &["entity_uri [str]"],
        &["--tier [urgent|high|normal...]", "--user [name]"],
        "Watch an entity so its notifications reach the personal mailbox; re-watching updates \
         the notification tiers in place (RAL-343).",
        false,
        false, // creates/updates a watch.
        &[],
    ),
    node(
        "unwatch",
        &["entity_uri [str]"],
        &["--user [name]"],
        "Stop watching an entity (RAL-343).",
        false,
        false, // deletes a watch.
        &[],
    ),
    node(
        "watches",
        &[],
        &["--user [name]"],
        "List the acting user's watches (RAL-343).",
        false,
        true, // read-only listing.
        &[],
    ),
    node(
        "preferences",
        &[],
        &["--user [name]"],
        "Show a user's notification preferences: automatic creator watches and default notify tiers.",
        false,
        true, // read-only.
        &[],
    ),
    node(
        "set-preferences",
        &[],
        &[
            "--user [name]",
            "--auto-watch",
            "--no-auto-watch",
            "--tier [urgent|high|normal...]",
        ],
        "Set a user's automatic-watch and default notification-tier preferences; requires exactly \
         one of --auto-watch/--no-auto-watch (RAL-320).",
        false,
        false, // mutates stored preferences.
        &[],
    ),
];

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
        "env",
        &["squad_id [str]"],
        &[],
        "List a squad's resolved environment variables, read-only (RAL-324); values of names registered in the Secrets tab are masked.",
        false,
        true, // ("squad", "env")
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
            "--auto-compact-threshold [tokens]",
            "--command [cmd]",
            "--cwd [path]",
            "--maximum-tool-output-tokens [tokens]",
            "--model [name]",
            "--prompt [text]",
            "--system-prompt [text]",
        ],
        "Edit a cell's fields.",
        false,
        false,
        &[],
    ),
    node(
        "env",
        &["selector [str]"],
        &["--scope [cell|proof]"],
        "List a cell's resolved environment variables, read-only (RAL-324); --scope proof shows what its own proof steps inherit.",
        false,
        true, // ("cell", "env")
        &[],
    ),
    node(
        "open-agent",
        &["selector [str]"],
        &[],
        "Open the real interactive agent in a new terminal -- while running, cleanly detaches the cell first (RAL-288); while finished, resumes it the old way.",
        false,
        false,
        &[],
    ),
    node(
        "remote-terminal",
        &["selector [str]"],
        &[],
        "Attach an interactive terminal to a remote cell's resumed Claude Code session over the daemon's WebSocket relay (RAL-355).",
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
        "resume-automation",
        &["selector [str]"],
        &[],
        "Hand a detached cell back to unattended execution, continuing the exact same agent conversation (RAL-288).",
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
        &["--name [name]", "--project [name]", "--model [name]"],
        "Edit a task node's name/project/model.",
        false,
        false,
        &[],
    ),
    node(
        "env",
        &["selector [str]"],
        &["--scope [task|proof]"],
        "List a task's resolved environment variables, read-only (RAL-324); --scope proof shows what its task-scoped proof steps inherit.",
        false,
        true, // ("task", "env")
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
        "edit",
        &["selector [str]"],
        &["--maximum-tool-output-tokens [tokens]", "--model [name]"],
        "Edit a proof step's model/tool-output-cap overrides.",
        false,
        false,
        &[],
    ),
    node(
        "env",
        &["selector [str]"],
        &[],
        "List a proof step's resolved environment variables, read-only (RAL-324); values of names registered in the Secrets tab are masked.",
        false,
        true, // ("proof", "env")
        &[],
    ),
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

const QUICK_START_BACKENDS: &[HelpNode] = &[
    node(
        "claude-code",
        &[],
        &["--command [cmd]", "--read-only", "--shell [shell]"],
        "Launch Claude Code with the selected Ralphus role prompt; arguments after `--` are forwarded verbatim.",
        false,
        false,
        &[],
    ),
    node(
        "codex",
        &[],
        &["--command [cmd]", "--read-only", "--shell [shell]"],
        "Launch Codex with the selected Ralphus role prompt; arguments after `--` are forwarded verbatim.",
        false,
        false,
        &[],
    ),
    node(
        "pi",
        &[],
        &["--command [cmd]", "--read-only", "--shell [shell]"],
        "Launch Pi with the selected Ralphus role prompt; arguments after `--` are forwarded verbatim.",
        false,
        false,
        &[],
    ),
];

const QUICK_START_REVIEWER_BACKENDS: &[HelpNode] = &[
    node(
        "claude-code",
        &["target [str, optional]"],
        &["--command [cmd]", "--read-only", "--shell [shell]"],
        "Launch Claude Code as a reviewer; arguments after `--` are forwarded verbatim.",
        false,
        false,
        &[],
    ),
    node(
        "codex",
        &["target [str, optional]"],
        &["--command [cmd]", "--read-only", "--shell [shell]"],
        "Launch Codex as a reviewer; arguments after `--` are forwarded verbatim.",
        false,
        false,
        &[],
    ),
    node(
        "pi",
        &["target [str, optional]"],
        &["--command [cmd]", "--read-only", "--shell [shell]"],
        "Launch Pi as a reviewer; arguments after `--` are forwarded verbatim.",
        false,
        false,
        &[],
    ),
];

/// User-facing but deliberately omitted from [`generate`], because an agent
/// already running inside quick-start must not recursively launch another
/// harness. It remains part of the mandatory invocation registry.
pub const QUICK_START: HelpNode = node(
    "quick-start",
    &[],
    &[],
    "Launch an interactive agent preconfigured for a Ralphus role.",
    false,
    false,
    &[
        node(
            "manager",
            &[],
            &[],
            "Launch an agent that can orchestrate Ralphus tasks.",
            false,
            false,
            QUICK_START_BACKENDS,
        ),
        node(
            "reviewer",
            &[],
            &[],
            "Launch an agent that operates an existing Guardian review.",
            false,
            false,
            QUICK_START_REVIEWER_BACKENDS,
        ),
        node(
            "watcher",
            &[],
            &[],
            "Launch an agent that monitors and drains the escalation mailbox.",
            false,
            false,
            QUICK_START_BACKENDS,
        ),
    ],
);

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
            "Drain the escalation mailbox (RAL-241): failed/stalled work the daemon flagged for \
             attention. Also personal watches and notification preferences layered over the \
             same mailbox (RAL-320).",
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
            "triage",
            &[],
            &[],
            "Register and inspect Triage types -- the Arbiter subsystem's automatic-review \
classification categories (RAL-318).",
            false,
            false,
            TRIAGE_CHILDREN,
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
fn render(n: &HelpNode, depth: usize, root_name: Option<&str>, out: &mut String) {
    let indent = "    ".repeat(depth);
    let mut chips: Vec<&str> = n.positionals.to_vec();
    let mut opts: Vec<&str> = n.options.to_vec();
    opts.sort_unstable();
    chips.extend(opts);
    let display_name = if depth == 0 {
        root_name.unwrap_or(n.name)
    } else {
        n.name
    };
    let head = if chips.is_empty() {
        display_name.to_string()
    } else {
        format!(
            "{} {}",
            display_name,
            chips
                .iter()
                .map(|chip| display_chip(chip))
                .collect::<Vec<_>>()
                .join(" ")
        )
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
        render(child, depth + 1, root_name, out);
    }
}

/// True if `n` itself is `(read-only-safe)`, or any descendant is -- a group
/// header (e.g. `review`, `cell`) is never tagged itself but must still be
/// printed by [`render_read_only_safe`] when it has tagged descendants, so
/// the filtered tree keeps the path prefix those subcommands need.
fn subtree_has_read_only_safe(n: &HelpNode) -> bool {
    n.read_only_safe || n.children.iter().any(subtree_has_read_only_safe)
}

/// Same output shape as [`render`], but prunes every branch that contains no
/// `(read-only-safe)` command -- the tree shown to a `--read-only`
/// quick-start session, so it only ever sees commands it's allowed to run.
fn render_read_only_safe(n: &HelpNode, depth: usize, root_name: Option<&str>, out: &mut String) {
    if !subtree_has_read_only_safe(n) {
        return;
    }
    let indent = "    ".repeat(depth);
    let mut chips: Vec<&str> = n.positionals.to_vec();
    let mut opts: Vec<&str> = n.options.to_vec();
    opts.sort_unstable();
    chips.extend(opts);
    let display_name = if depth == 0 {
        root_name.unwrap_or(n.name)
    } else {
        n.name
    };
    let head = if chips.is_empty() {
        display_name.to_string()
    } else {
        format!(
            "{} {}",
            display_name,
            chips
                .iter()
                .map(|chip| display_chip(chip))
                .collect::<Vec<_>>()
                .join(" ")
        )
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
    let mut children: Vec<&HelpNode> = n
        .children
        .iter()
        .filter(|c| subtree_has_read_only_safe(c))
        .collect();
    children.sort_by_key(|c| c.name);
    for child in children {
        render_read_only_safe(child, depth + 1, root_name, out);
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

fn find_registered_node(path: &[&str]) -> Option<&'static HelpNode> {
    match path.split_first() {
        Some((head, tail)) if *head == "quick-start" => find_node(&QUICK_START, tail),
        _ => find_node(&ROOT, path),
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
        format!(
            "{} {}",
            n.name,
            chips
                .iter()
                .map(|chip| display_chip(chip))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}

fn display_chip(chip: &str) -> String {
    let name = chip.split_whitespace().next().unwrap_or(chip);
    if matches!(name, "selector" | "entity_uri" | "--entity" | "--for") {
        chip.replacen("[str", "[uri", 1)
    } else {
        chip.to_string()
    }
}

fn has_uri_chip(chips: impl IntoIterator<Item = &'static str>) -> bool {
    chips
        .into_iter()
        .any(|chip| display_chip(chip).contains("[uri"))
}

fn command_path(path: &[&str]) -> String {
    let program = crate::program_name::resolve_program_name();
    if path.is_empty() {
        program
    } else {
        format!("{program} {}", path.join(" "))
    }
}

#[must_use]
pub fn command_help(path: &[&str]) -> Option<String> {
    let node = find_registered_node(path)?;
    let full = command_path(path);
    let mut out = String::new();
    out.push_str(&format!("{full} -- {}\n", node.description));
    out.push_str("USAGE:\n    ");
    out.push_str(&full);
    if !node.positionals.is_empty() {
        out.push(' ');
        out.push_str(
            &node
                .positionals
                .iter()
                .map(|chip| display_chip(chip))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    if !node.options.is_empty() {
        out.push_str(" [OPTIONS]");
    }
    if !node.children.is_empty() {
        out.push_str(" <SUBCOMMAND> [ARGS...]");
    }
    out.push('\n');
    if !node.positionals.is_empty() {
        out.push_str("\nARGUMENTS:\n");
        for positional in node.positionals {
            let displayed = display_chip(positional);
            out.push_str(&format!(
                "    {:<32} {}\n",
                displayed,
                chip_description(positional, false)
            ));
        }
    }
    {
        let mut opts: Vec<&str> = node.options.to_vec();
        opts.sort_unstable();
        out.push_str("\nOPTIONS:\n");
        for opt in opts {
            let displayed = display_chip(opt);
            out.push_str(&format!(
                "    {displayed:<32} {}\n",
                chip_description(opt, true)
            ));
        }
        out.push_str("    -h, --help                       Print help and exit\n");
    }
    if has_uri_chip(
        node.positionals
            .iter()
            .copied()
            .chain(node.options.iter().copied()),
    ) {
        out.push_str("\nURI ARGUMENTS:\n    ");
        out.push_str(URI_ARGUMENT_NOTE);
        out.push('\n');
    }
    if !node.children.is_empty() || path.is_empty() {
        let mut children: Vec<&HelpNode> = node.children.iter().collect();
        if path.is_empty() {
            children.push(&QUICK_START);
        }
        children.sort_by_key(|child| child.name);
        out.push_str("\nSUBCOMMANDS:\n");
        for child in children {
            out.push_str(&format!(
                "    {}  {}\n",
                signature(child),
                child.description
            ));
        }
    }
    Some(out.trim_end().to_string())
}

/// The `selector [uri]` chip's grammar, spelled out with concrete examples
/// (RAL-376) -- both the legacy path form (`cli/src/selector.rs`'s
/// `parse_squad_selector`) and the RAL-188 URI form
/// (`core/src/uri.rs`) resolve to the same squad/task/cell/proof
/// coordinates.
const SELECTOR_GRAMMAR: &str = "e.g. squad-000000000001/build/0 (squad/task/cell path) or \
squad-000000000001/build/proof/0 (proof path); also accepts the RAL-188 URI form, e.g. \
ralphus:/SQUAD[my squad]/TASK[build]?id=squad-000000000001";

/// The `entity_uri [uri]` chip's grammar, mirrored from
/// `daemon/src/entity_uri.rs`/`cli/src/entity_uri.rs`'s `EntityUri` (RAL-155)
/// -- kept in sync with that grammar by hand since it has no shared constant
/// of its own to import here.
const ENTITY_URI_GRAMMAR: &str = "a colon-separated entity URI: squad:<squad_id>, \
task:<squad_id>:<task_idx>, cell:<squad_id>:<task_idx>:<cell_idx>, \
proof:<squad_id>:<task_idx>:<task|cell>:<cell_idx>:<proof_idx> (cell_idx is -1 for a \
task-scope proof), or guardian:<guardian_id>, e.g. task:squad-000000000001:0 or \
proof:squad-000000000001:0:cell:0:1";

/// Free-text description of one chip's grammar, keyed by its bare name --
/// used by [`command_help`] to build `ralphus <cmd> --help`'s
/// `ARGUMENTS:`/`OPTIONS:` sections, and by `ralphus-mcp`'s tool-schema
/// generation (`mcp/src/tools.rs::chip_schema`) so the same grammar/example
/// text shows up in an MCP tool's JSON-Schema `description`, per RAL-376.
#[must_use]
pub fn chip_description(chip: &str, option: bool) -> String {
    let name = chip.split_whitespace().next().unwrap_or(chip);
    let key = name.trim_start_matches('-').replace('-', " ");
    if option {
        match name {
            "--daemon-url" => "Base URL of the Ralphus daemon.".to_string(),
            "--json" => "Emit machine-readable JSON instead of human output.".to_string(),
            "--version" => "Print the version and exit.".to_string(),
            "--command" => "Override the agent harness command for this launch.".to_string(),
            "--shell" => "Shell used to interpret the command override.".to_string(),
            "--read-only" => "Launch the agent with read-only restrictions.".to_string(),
            "--squad" => "Filter to this exact squad id, e.g. squad-000000000001.".to_string(),
            "--guardian" => {
                "Filter to this exact guardian (review) id, e.g. guardian-000000000001.".to_string()
            }
            "--entity" => format!("Filter to this exact entity URI: {ENTITY_URI_GRAMMAR}."),
            "--for" => format!(
                "Resolve this squad/task/cell/proof selector and filter to its entity URI, \
                 {SELECTOR_GRAMMAR}."
            ),
            _ if chip.contains('[') => format!("Set the {key} value using the shown value type."),
            _ => format!("Enable {key}."),
        }
    } else {
        match name {
            "selector" => {
                format!("Entity selector or URI identifying the target, {SELECTOR_GRAMMAR}.")
            }
            "squad_id" => {
                "Squad identifier or accepted squad selector, e.g. squad-000000000001.".to_string()
            }
            "entity_uri" => {
                format!("Entity URI addressing any node uniformly: {ENTITY_URI_GRAMMAR}.")
            }
            "pr_id" => "Pull request identifier, e.g. pr-000000000001.".to_string(),
            "target" => "Optional review target supplied to the launched reviewer.".to_string(),
            "file" => "Input file path; repeat where the usage permits it.".to_string(),
            "field" => "Optional dotted field path to extract from the entity JSON.".to_string(),
            "text" => "Text content sent to the selected operation.".to_string(),
            "state" => "Destination state accepted by the selected entity type.".to_string(),
            _ => format!("{key} value using the type shown in usage."),
        }
    }
}

fn command_tokens(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        if arg == "--json" || arg == "--version" {
            i += 1;
            continue;
        }
        if arg == "--daemon-url" {
            i += 2;
            continue;
        }
        if arg.starts_with("--daemon-url=") || matches!(arg, "--help" | "-h") {
            i += 1;
            continue;
        }
        out.push(arg);
        i += 1;
    }
    out
}

fn resolved_path(args: &[String], strict: bool) -> Result<Vec<&str>, String> {
    let tokens = command_tokens(args);
    let Some(first) = tokens.first().copied() else {
        return Ok(Vec::new());
    };
    if first == "help" {
        return Ok(Vec::new());
    }
    let mut path = vec![first];
    let mut node =
        find_registered_node(&path).ok_or_else(|| format!("unknown command: {first}"))?;
    let mut index = 1;
    while !node.children.is_empty() && index < tokens.len() {
        let candidate = tokens[index];
        if candidate == "help" {
            return Ok(path);
        }
        let Some(child) = node.children.iter().find(|child| child.name == candidate) else {
            if strict {
                return Err(format!(
                    "unknown {} subcommand: {candidate}",
                    path.join(" ")
                ));
            }
            break;
        };
        path.push(candidate);
        node = child;
        index += 1;
    }
    Ok(path)
}

/// Returns the requested help screen before any ordinary parsing or work.
/// A bare `--` ends Ralphus option handling, so later help flags are passed
/// through to the downstream command unchanged.
#[must_use]
pub fn requested_help(args: &[String]) -> Option<String> {
    let help = args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"));
    if !help {
        return None;
    }
    let path = resolved_path(args, false).unwrap_or_default();
    command_help(&path)
}

/// Validates the command-prefix portion of an invocation against the same
/// registry that renders help. This is the structural guard: adding parser
/// code alone cannot expose a new command without a registered help node.
pub fn validate_invocation(args: &[String]) -> Result<(), String> {
    resolved_path(args, true).map(|_| ())
}

/// Every user-callable command path, used by exhaustive invariant tests.
#[must_use]
pub fn registered_paths() -> Vec<Vec<&'static str>> {
    fn walk(
        node: &'static HelpNode,
        path: &mut Vec<&'static str>,
        out: &mut Vec<Vec<&'static str>>,
    ) {
        out.push(path.clone());
        for child in node.children {
            path.push(child.name);
            walk(child, path, out);
            path.pop();
        }
    }
    let mut out = vec![Vec::new()];
    for child in ROOT.children.iter().chain(std::iter::once(&QUICK_START)) {
        let mut path = vec![child.name];
        walk(child, &mut path, &mut out);
    }
    out
}

/// Every *leaf* command path (a node with no children -- i.e. an actual
/// runnable command, not a group header like bare `task`), paired with its
/// full [`HelpNode`] (positionals, options, description, `read_only_safe`,
/// ...). This is the cross-check anchor used by `cli`'s
/// `commands::parse_args`-drift test and the MCP parity/tool-generation code
/// in the `ralphus-mcp` crate: `help_map.rs`'s tree is currently
/// hand-authored in parallel with the real command surface in
/// `commands/*.rs`'s `parse()` functions, so nothing stops the two from
/// drifting apart -- a node added here with no matching parser arm, or a
/// parser arm added with no matching node here, both compile cleanly today.
/// Walking this list and actually invoking `commands::parse_args` on each
/// path is how that drift gets caught before it reaches CI, not after.
#[must_use]
pub fn registered_leaves() -> Vec<(Vec<&'static str>, &'static HelpNode)> {
    fn walk(
        node: &'static HelpNode,
        path: &mut Vec<&'static str>,
        out: &mut Vec<(Vec<&'static str>, &'static HelpNode)>,
    ) {
        if node.children.is_empty() {
            out.push((path.clone(), node));
        }
        for child in node.children {
            path.push(child.name);
            walk(child, path, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    for child in ROOT.children.iter().chain(std::iter::once(&QUICK_START)) {
        let mut path = vec![child.name];
        walk(child, &mut path, &mut out);
    }
    out
}

/// The full alphabetized, indented help-map tree, as one string -- ports
/// `helpmap.py::generate()`.
#[must_use]
pub fn generate() -> String {
    let mut out = String::new();
    let program = crate::program_name::resolve_program_name();
    render(&ROOT, 0, Some(&program), &mut out);
    // `render` appends a trailing "\n" after every line (including the
    // last); Python's `"\n".join(lines)` has no trailing newline, so trim it
    // to match exactly.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// [`generate()`]'s tree, pruned to only `(read-only-safe)` commands (and the
/// group headers needed to reach them) -- what a `--read-only` quick-start
/// session is shown, so it can't discover a mutating command it isn't
/// allowed to call in the first place.
#[must_use]
pub fn generate_read_only_safe() -> String {
    let mut out = String::new();
    let program = crate::program_name::resolve_program_name();
    render_read_only_safe(&ROOT, 0, Some(&program), &mut out);
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// The eight guidance notes, blank-line separated, followed by [`generate()`]'s
/// tree -- ports `helpmap.py::main()`'s exact print sequence (plus
/// [`SUBMIT_RETRY_NOTE`], added after the Python port). This is what
/// `ralphus show help-map` prints.
#[must_use]
pub fn full_output() -> String {
    crate::program_name::substitute_backticked_invocations(&format!(
        "{SUBAGENT_NOTE}\n\n{READ_ONLY_NOTE}\n\n{PROJECT_LOOKUP_NOTE}\n\n{SUBMIT_VALIDATE_NOTE}\n\n\
{SUBMIT_REVIEW_NOTE}\n\n{SUBMIT_RETRY_NOTE}\n\n{JSON_NOTE}\n\n{URI_ARGUMENT_NOTE}\n\n{}",
        generate()
    ))
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

    const MUTATING_CHILD: HelpNode =
        node("gamma", &[], &[], "does gamma things", false, false, &[]);

    const MIXED_ROOT: HelpNode = node(
        "root",
        &[],
        &[],
        "root desc",
        false,
        false,
        &[SAMPLE_CHILD, MUTATING_CHILD],
    );

    #[test]
    fn renders_indentation_and_child_nesting() {
        let mut out = String::new();
        render(&SAMPLE_ROOT, 0, None, &mut out);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("    - "));
    }

    #[test]
    fn sorts_option_chips_alphabetically_but_keeps_positional_order_first() {
        let mut out = String::new();
        render(&SAMPLE_CHILD, 0, None, &mut out);
        // positional first, then options alphabetized (--beta before --zeta)
        assert!(out.contains("alpha pos1 --beta [val] --zeta"));
    }

    #[test]
    fn read_only_safe_prefix_comes_before_name_subagent_after_chips() {
        let mut out = String::new();
        render(&SAMPLE_CHILD, 0, None, &mut out);
        assert!(out.starts_with("- (read-only-safe) alpha "));
        let mut out2 = String::new();
        render(&SAMPLE_ROOT, 0, None, &mut out2);
        assert!(out2.starts_with("- root --json (subagent)  {root desc}"));
    }

    #[test]
    fn render_read_only_safe_keeps_untagged_group_header_with_tagged_descendant() {
        let mut out = String::new();
        render_read_only_safe(&MIXED_ROOT, 0, None, &mut out);
        // Untagged "root" is kept (it's the path prefix "alpha" needs), tagged
        // "alpha" is kept, untagged "gamma" (no read-only-safe descendant) is pruned.
        assert!(out.contains("- root  {root desc}"));
        assert!(out.contains("(read-only-safe) alpha"));
        assert!(!out.contains("gamma"));
    }

    #[test]
    fn render_read_only_safe_drops_whole_subtree_with_no_tagged_descendant() {
        let mut out = String::new();
        render_read_only_safe(&MUTATING_CHILD, 0, None, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn description_is_wrapped_in_braces() {
        let mut out = String::new();
        render(&SAMPLE_CHILD, 0, None, &mut out);
        assert!(out.contains("{does alpha things}"));
    }

    #[test]
    fn generate_has_no_trailing_newline() {
        let text = generate();
        assert!(!text.ends_with('\n'));
        assert!(text.starts_with("- ralphus"));
    }

    #[test]
    fn generate_read_only_safe_excludes_mutating_leaves_but_keeps_their_group_headers() {
        let text = generate_read_only_safe();
        assert!(!text.ends_with('\n'));
        assert!(text.starts_with("- ralphus"));
        // "review show" is read-only-safe; "review" itself never carries the tag
        // but must still appear as the path prefix "show" needs.
        assert!(text.contains("- review "));
        assert!(text.contains("(read-only-safe) show"));
        // "submit" and "review merge" are mutating and have no read-only-safe
        // sibling reachable through them, so they're pruned entirely.
        assert!(!text.contains("submit "));
        assert!(!text.contains("merge selector"));
    }

    #[test]
    fn full_output_prints_all_eight_notes_before_the_tree() {
        let text = full_output();
        assert!(text.starts_with(SUBAGENT_NOTE));
        for note in [
            SUBAGENT_NOTE,
            READ_ONLY_NOTE,
            PROJECT_LOOKUP_NOTE,
            SUBMIT_VALIDATE_NOTE,
            SUBMIT_REVIEW_NOTE,
            SUBMIT_RETRY_NOTE,
            JSON_NOTE,
            URI_ARGUMENT_NOTE,
        ] {
            assert!(
                text.contains(&crate::program_name::substitute_backticked_invocations(
                    note
                ))
            );
        }
        assert!(text.contains("- ralphus"));
    }

    #[test]
    fn rendered_uri_arguments_use_uri_chips_and_document_every_entity_kind() {
        let map = generate();
        assert!(map.contains("- cell"));
        assert!(map.contains("        - edit selector [uri]"));
        assert!(map.contains("--entity [uri] --for [uri]"));
        for raw_uri_chip in [
            "selector [str]",
            "entity_uri [str]",
            "--entity [str]",
            "--for [str]",
        ] {
            assert!(
                !map.contains(raw_uri_chip),
                "URI argument still rendered as a string: {raw_uri_chip}"
            );
        }

        let full = full_output();
        for example in [
            "squad:squad-1",
            "task:squad-1:2",
            "cell:squad-1:2:0",
            "proof:squad-1:2:cell:0:1",
            "guardian:g-1",
        ] {
            assert!(full.contains(example), "missing URI example: {example}");
        }

        let command = command_help(&["task", "show"]).expect("task show help");
        assert!(command.contains("selector [uri]"));
        assert!(command.contains("URI ARGUMENTS:"));
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
        let program = crate::program_name::resolve_program_name();
        assert!(text.contains(&format!(
            "USAGE:\n    {program} [OPTIONS] <SUBCOMMAND> [ARGS...]"
        )));
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
        assert!(text.contains(&format!(
            "{} review pr --",
            crate::program_name::resolve_program_name()
        )));
        assert!(text.contains("SUBCOMMANDS:"));
        assert!(text.contains("comments"));
        assert!(text.contains("pull-feedback"));
        assert!(text.contains("submit"));
    }

    #[test]
    fn quick_start_manager_has_discoverable_backend_help() {
        let text = command_help(&["quick-start", "manager"]).expect("manager help");
        assert!(text.contains("claude-code"));
        assert!(text.contains("codex"));
        assert!(text.contains("pi"));
    }

    #[test]
    fn every_registered_path_has_detailed_help_and_universal_precedence() {
        for path in registered_paths() {
            let text = command_help(&path).unwrap_or_else(|| panic!("missing help for {path:?}"));
            assert!(text.contains("USAGE:"), "incomplete help for {path:?}");
            assert!(
                text.contains("-h, --help"),
                "missing help flag for {path:?}"
            );

            let mut argv: Vec<String> = path.iter().map(|part| (*part).to_string()).collect();
            argv.extend(["--definitely-invalid".to_string(), "--help".to_string()]);
            assert_eq!(
                requested_help(&argv).as_deref(),
                Some(text.as_str()),
                "help did not take precedence for {path:?}"
            );
        }
    }

    #[test]
    fn registry_gates_unknown_subcommands_but_help_uses_deepest_known_parent() {
        let argv = ["quick-start", "manager", "future-backend"]
            .map(str::to_string)
            .to_vec();
        assert!(validate_invocation(&argv).is_err());

        let mut with_help = argv;
        with_help.push("--help".to_string());
        let text = requested_help(&with_help).expect("parent help");
        assert!(text.starts_with(&format!(
            "{} quick-start manager --",
            crate::program_name::resolve_program_name()
        )));
    }

    #[test]
    fn help_after_passthrough_separator_is_not_intercepted() {
        let argv = ["quick-start", "manager", "codex", "--", "--help"]
            .map(str::to_string)
            .to_vec();
        assert!(requested_help(&argv).is_none());
    }
}
