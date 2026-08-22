//! `ralphus quick-start <manager|reviewer|watcher> <claude-code|codex>`.
//! `manager`/`reviewer` are ported from `cli/src/ralphus/__main__.py`'s
//! quick-start group; `watcher` is new (RAL-241) -- a mailbox-polling
//! supervisor session that registers a `client_id` with the daemon on
//! startup (see `crate::commands::mailbox::ensure_client_id`) and launches
//! with a system prompt instructing the agent to run `ralphus mailbox check`
//! after every user turn.
//!
//! Despite the "interactive" framing, this does **not** touch tmux/psmux at
//! all -- that mechanism is for tracking a *scheduled task's* session, a
//! completely different code path. Quick-start just resolves a shell/program
//! (reusing `ralphus_runner::shellcmd`, already ported and shared with the
//! runner), writes a system-prompt temp file, and does one plain foreground
//! subprocess spawn that inherits this process's own stdin/stdout/stderr
//! directly (`std::process::Command::status()`, matching Python's
//! `subprocess.run(args, check=False)` with no captured stdio) -- the
//! spawned `claude`/`codex` process IS the user's terminal session from
//! that point on.

use std::process::Command;

use crate::args::GlobalOpts;
use crate::flags::Scanner;

const CLAUDE_READ_ONLY_MECHANISM: &str = "--permission-mode plan";
const CODEX_READ_ONLY_MECHANISM: &str = "--sandbox read-only";

#[derive(Debug, Clone, Default)]
pub struct LaunchArgs {
    pub command: Option<String>,
    pub shell: Option<String>,
    pub read_only: bool,
    pub passthrough: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum QuickStartCommand {
    Help,
    ManagerHelp,
    ManagerClaudeCode(LaunchArgs),
    ManagerCodex(LaunchArgs),
    ReviewerHelp,
    ReviewerClaudeCode {
        target: Option<String>,
        args: LaunchArgs,
    },
    ReviewerCodex {
        target: Option<String>,
        args: LaunchArgs,
    },
    /// RAL-241: a mailbox-polling supervisor session (see
    /// `crate::commands::mailbox`) -- registers a `client_id` with the
    /// daemon on startup, then launches with a system prompt instructing the
    /// agent to run `ralphus mailbox check` after every user turn.
    WatcherHelp,
    WatcherClaudeCode(LaunchArgs),
    WatcherCodex(LaunchArgs),
    UsageError(String),
}

/// Splits `args` at the first bare `--`, matching Python's
/// `_split_passthrough`: only `quick-start` gets this treatment, so a
/// command line typed at a shell prompt (and forwarded verbatim to
/// `claude`/`codex`) is never flag-scanned by this CLI's own parser.
fn split_passthrough(args: &[String]) -> (Vec<String>, Vec<String>) {
    match args.iter().position(|a| a == "--") {
        Some(idx) => (args[..idx].to_vec(), args[idx + 1..].to_vec()),
        None => (args.to_vec(), Vec::new()),
    }
}

fn parse_launch_args(args: &[String], passthrough: Vec<String>) -> (LaunchArgs, Vec<String>) {
    let mut scanner = Scanner::new(args);
    let command = scanner.take_value("--command").ok().flatten();
    let shell = scanner.take_value("--shell").ok().flatten();
    let read_only = scanner.take_bool("--read-only");
    let rest = scanner.remaining();
    (
        LaunchArgs {
            command,
            shell,
            read_only,
            passthrough,
        },
        rest,
    )
}

#[must_use]
pub fn parse(args: &[String]) -> QuickStartCommand {
    let (head, passthrough) = split_passthrough(args);
    match head.first().map(String::as_str) {
        None => QuickStartCommand::Help,
        Some("manager") => match head.get(1).map(String::as_str) {
            None => QuickStartCommand::ManagerHelp,
            Some("claude-code") => {
                let (launch, _) = parse_launch_args(&head[2..], passthrough);
                QuickStartCommand::ManagerClaudeCode(launch)
            }
            Some("codex") => {
                let (launch, _) = parse_launch_args(&head[2..], passthrough);
                QuickStartCommand::ManagerCodex(launch)
            }
            Some(other) => QuickStartCommand::UsageError(format!(
                "unknown quick-start manager subcommand: {other}"
            )),
        },
        Some("reviewer") => match head.get(1).map(String::as_str) {
            None => QuickStartCommand::ReviewerHelp,
            Some("claude-code") => {
                let (launch, rest) = parse_launch_args(&head[2..], passthrough);
                QuickStartCommand::ReviewerClaudeCode {
                    target: rest.into_iter().next(),
                    args: launch,
                }
            }
            Some("codex") => {
                let (launch, rest) = parse_launch_args(&head[2..], passthrough);
                QuickStartCommand::ReviewerCodex {
                    target: rest.into_iter().next(),
                    args: launch,
                }
            }
            Some(other) => QuickStartCommand::UsageError(format!(
                "unknown quick-start reviewer subcommand: {other}"
            )),
        },
        Some("watcher") => match head.get(1).map(String::as_str) {
            None => QuickStartCommand::WatcherHelp,
            Some("claude-code") => {
                let (launch, _) = parse_launch_args(&head[2..], passthrough);
                QuickStartCommand::WatcherClaudeCode(launch)
            }
            Some("codex") => {
                let (launch, _) = parse_launch_args(&head[2..], passthrough);
                QuickStartCommand::WatcherCodex(launch)
            }
            Some(other) => QuickStartCommand::UsageError(format!(
                "unknown quick-start watcher subcommand: {other}"
            )),
        },
        Some(other) => {
            QuickStartCommand::UsageError(format!("unknown quick-start subcommand: {other}"))
        }
    }
}

#[must_use]
#[allow(clippy::print_stdout)]
pub fn dispatch(cmd: QuickStartCommand, opts: &GlobalOpts) -> i32 {
    match cmd {
        QuickStartCommand::Help => {
            println!(
                "ralphus quick-start <manager|reviewer|watcher> <claude-code|codex> [--command CMD] [--shell SHELL] [--read-only] [-- ARGS...]"
            );
            0
        }
        QuickStartCommand::ManagerHelp => {
            println!("ralphus quick-start manager <claude-code|codex>");
            0
        }
        QuickStartCommand::ReviewerHelp => {
            println!("ralphus quick-start reviewer <claude-code|codex> [target]");
            0
        }
        QuickStartCommand::WatcherHelp => {
            println!("ralphus quick-start watcher <claude-code|codex>");
            0
        }
        QuickStartCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        QuickStartCommand::ManagerClaudeCode(launch) => launch_claude(
            "quick-start-manager-claude-code",
            manager_system_prompt_content(launch.read_only, CLAUDE_READ_ONLY_MECHANISM),
            &launch,
        ),
        QuickStartCommand::ManagerCodex(launch) => launch_codex(
            "quick-start-manager-codex",
            manager_system_prompt_content(launch.read_only, CODEX_READ_ONLY_MECHANISM),
            &launch,
        ),
        QuickStartCommand::ReviewerClaudeCode { target, args } => launch_claude(
            "quick-start-reviewer-claude-code",
            reviewer_system_prompt_content(
                target.as_deref(),
                args.read_only,
                CLAUDE_READ_ONLY_MECHANISM,
            ),
            &args,
        ),
        QuickStartCommand::ReviewerCodex { target, args } => launch_codex(
            "quick-start-reviewer-codex",
            reviewer_system_prompt_content(
                target.as_deref(),
                args.read_only,
                CODEX_READ_ONLY_MECHANISM,
            ),
            &args,
        ),
        QuickStartCommand::WatcherClaudeCode(launch) => {
            if ensure_watcher_registered(opts).is_none() {
                return 2;
            }
            launch_claude(
                "quick-start-watcher-claude-code",
                watcher_system_prompt_content(launch.read_only, CLAUDE_READ_ONLY_MECHANISM),
                &launch,
            )
        }
        QuickStartCommand::WatcherCodex(launch) => {
            if ensure_watcher_registered(opts).is_none() {
                return 2;
            }
            launch_codex(
                "quick-start-watcher-codex",
                watcher_system_prompt_content(launch.read_only, CODEX_READ_ONLY_MECHANISM),
                &launch,
            )
        }
    }
}

/// RAL-241: register (or reuse a previously persisted) mailbox `client_id`
/// before launching a watcher session, so `ralphus mailbox check` -- which
/// the watcher's system prompt instructs the agent to run after every user
/// turn -- has an id to poll with from its very first invocation, with no
/// manual setup step. `None` (having already printed an error) if
/// registration fails; the caller aborts the launch in that case, since a
/// watcher session that can never drain the mailbox defeats the point of
/// this command.
fn ensure_watcher_registered(opts: &GlobalOpts) -> Option<()> {
    match crate::commands::mailbox::ensure_client_id(&opts.client()) {
        Ok(client_id) => {
            eprintln!("ralphus [runner] quick-start-watcher mailbox client_id={client_id}");
            Some(())
        }
        Err(e) => {
            e.print(opts.json, None);
            None
        }
    }
}

// ---- system prompt composition ------------------------------------------

fn read_only_session_note(mechanism: &str) -> String {
    format!(
        "READ-ONLY MODE: this session was launched with `--read-only`. You can freely read and \
         look at anything -- browse and read files, and call any `ralphus` subcommand tagged \
         `(read-only-safe)` in the help-map below (see also its own `READ_ONLY_NOTE`) to inspect \
         runs, tasks, reviews, and other state; none of that data changes as a result. Do not \
         take any mutating action: no writing or editing files, no running a command that \
         changes state, and no invoking a mutating `ralphus` subcommand (submit, review \
         create/merge/approve/feedback/..., run/task/session/verify set-status/restart/edit/..., \
         clear, machine register/remove, project git, initialize git, etc.). This launch also \
         passed `{mechanism}` to enforce the same boundary at the harness-permission level -- \
         honor this guidance directly too, as defense-in-depth, rather than assuming the harness \
         alone will stop you."
    )
}

fn manager_system_prompt_content(read_only: bool, harness_mechanism: &str) -> String {
    let read_only_block = if read_only {
        format!("\n\n{}", read_only_session_note(harness_mechanism))
    } else {
        String::new()
    };
    format!(
        "You are Ralphus. You orchestrate the `ralphus` CLI as an autonomous agent. Its complete \
         command surface -- every subcommand, flag, and expected value type -- is documented \
         below. Use `ralphus <command> --help` for details on any specific command.\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}{}\n\n{}",
        crate::help_map::SUBAGENT_NOTE,
        crate::help_map::READ_ONLY_NOTE,
        crate::help_map::PROJECT_LOOKUP_NOTE,
        crate::help_map::SUBMIT_VALIDATE_NOTE,
        crate::help_map::SUBMIT_REVIEW_NOTE,
        crate::help_map::JSON_NOTE,
        read_only_block,
        crate::help_map::generate(),
    )
}

/// Shared framing for both reviewer quick-start harnesses: the agent
/// operates an *existing* review through the CLI's own `review ...` surface,
/// not a fresh ralphus orchestration session.
const REVIEWER_ROLE_NOTE: &str = "You are Ralphus operating in REVIEWER mode. You are not orchestrating new ralphus tasks -- you are acting as a human reviewer would inside the ralphus Guardian review board (the web board's Reviews tab), but through the `ralphus review ...` CLI surface instead of a browser. A review (also called a 'guardian') is a stack of one or more branches being rebased onto a base branch, with per-branch and combined feedback, checks, and merge control.\n\nReview operations you should be ready to perform on request, all via `ralphus review ...` (run `ralphus review --help` / `ralphus review <sub> --help` for exact flags):\n  - `review show <selector>` / `review status <selector>` / `review logs <selector>` -- inspect a review's current state and audit trail.\n  - `review feedback <guardian#branch> \"...\"` -- feedback on one branch, triggering a resolver re-attempt.\n  - `review chat send <selector> \"...\"` / `review chat show <selector>` -- combined/global review feedback thread (not tied to one branch).\n  - `review merge <selector>` / `review restart-merge <selector>` -- start or restart the stacked rebase.\n  - `review branch enable <guardian#branch>` / `review branch disable <guardian#branch>` -- enable/disable one branch in the stack.\n  - `review base list <selector>` / `review base set <selector> <branch>` -- inspect/change the base branch.\n  - `review checks list <selector>` / `review checks run <selector> [--index N | --all]` -- the review's manual (lint/format/test) checks; `checks run` PRINTS the command(s) + cwd rather than silently executing writes on your behalf.\n  - `review action list <selector>` / `review action run <selector> --index N` -- user-declared `[[review.action]]` test/action hints, same print-don't-run shape as checks.\n  - `review worktrees <selector>` -- the branches/worktrees a review consumes.\n  - `review list` -- list all reviews; use this (or a fresh `review show <selector>`) to switch to a different review at any point in this conversation -- you do not need to be relaunched to change which review you're operating on.\n\nRemote-state caution: do not assume the review's code lives on this machine. The daemon (reachable via --daemon-url, defaulting to local) is the authoritative source of truth for review state -- the worktree paths `review worktrees` reports may live on a different host than this one. Prefer the CLI's own `--json` views and the printed check/action commands over assuming a local git checkout. Only run a shell command directly against review code if you have independently confirmed the relevant path exists in your own local filesystem, and even then keep any such command read-only (inspection, never mutation).\n\nWrite boundary: every review mutation (feedback, merge/restart-merge, branch enable/disable, base-branch change, settings) must go through an explicit `ralphus review ...` subcommand. Never edit, commit, or push directly inside an inspected worktree -- that bypasses the review's own gating and audit trail.";

/// Resolves a reviewer quick-start TARGET to a guardian selector: a raw
/// selector as-is, or (best-effort) the id extracted from a
/// `.../reviews/<id>` URL path/fragment segment.
fn parse_review_target(target: &str) -> String {
    if !target.contains("://") {
        return target.to_string();
    }
    let Some(idx) = target.find("reviews/") else {
        return target.to_string();
    };
    let rest = &target[idx + "reviews/".len()..];
    let end = rest.find(['/', '?', '&', '#']).unwrap_or(rest.len());
    rest[..end].to_string()
}

fn reviewer_system_prompt_content(
    target: Option<&str>,
    read_only: bool,
    harness_mechanism: &str,
) -> String {
    let target_note = target.map_or_else(String::new, |t| {
        let selector = parse_review_target(t);
        format!("\n\nInitial review target for this session: `{selector}`. Start by running `ralphus review show {selector}` to confirm it still resolves before acting on it.")
    });
    let read_only_block = if read_only {
        format!("\n\n{}", read_only_session_note(harness_mechanism))
    } else {
        String::new()
    };
    format!(
        "You are Ralphus. The complete `ralphus` CLI command surface -- every subcommand, flag, \
         and expected value type -- is documented below for reference. Use `ralphus <command> \
         --help` for details on any specific command.\n\n{REVIEWER_ROLE_NOTE}{target_note}\n\n{}{read_only_block}\n\n{}",
        crate::help_map::READ_ONLY_NOTE,
        crate::help_map::generate(),
    )
}

/// RAL-241: framing for the watcher quick-start harness -- a mailbox-polling
/// supervisor rather than a fresh orchestration session or a review
/// operator. The one hard requirement (drain after every user turn) is
/// stated up front and repeated as "MANDATORY" since it's the entire reason
/// this quick-start variant exists; everything else is the same full
/// command surface `manager` gets, since a watcher may still need to
/// inspect/act on the squad/task/review the escalation is about.
const WATCHER_ROLE_NOTE: &str = "You are Ralphus operating in WATCHER mode (RAL-241). Your job is to supervise autonomous ralphus work by draining its escalation mailbox -- a queue of `urgent`/`high`/`normal` priority messages the daemon writes when something needs attention (a task/cell failed, or a session appears to have stalled with no activity for several minutes).\n\nMANDATORY: after every user turn -- i.e. as the first thing you do once you finish responding to what the user just asked, before going idle waiting for their next message -- run `ralphus mailbox check` and show its output to the user verbatim, even if it reports no unread messages. Do not silently swallow or summarize away a message.\n\nHow to react once you've shown a message:\n  - `urgent` -- stop and read it now. Treat it as more important than whatever else you were about to say; investigate it (e.g. `ralphus get <selector>`, `ralphus cell show <selector>`, `ralphus cartographer --squad-id <id>`) before continuing.\n  - `high` -- process it before you would otherwise go idle; it does not need to interrupt an in-progress response, but must not be left unaddressed.\n  - `normal` -- informational; mention it, no action required.\n\n`ralphus mailbox check` also marks whatever it returns as read (drained), so only genuinely new escalations appear on each subsequent check -- you do not need to deduplicate against earlier turns yourself.";

fn watcher_system_prompt_content(read_only: bool, harness_mechanism: &str) -> String {
    let read_only_block = if read_only {
        format!("\n\n{}", read_only_session_note(harness_mechanism))
    } else {
        String::new()
    };
    format!(
        "You are Ralphus. The complete `ralphus` CLI command surface -- every subcommand, flag, \
         and expected value type -- is documented below for reference. Use `ralphus <command> \
         --help` for details on any specific command.\n\n{WATCHER_ROLE_NOTE}\n\n{}{read_only_block}\n\n{}",
        crate::help_map::READ_ONLY_NOTE,
        crate::help_map::generate(),
    )
}

// ---- system-prompt-file merging ------------------------------------------

/// Combines ralphus's own system-prompt content with the contents of any
/// file the user forwarded via their own `--append-system-prompt-file` in
/// `--` passthrough args -- ralphus's content always first, wrapped in a
/// disclaimer telling the model to prefer it on conflict. `None` (having
/// already printed an error) if the user's file can't be read.
fn merge_append_system_prompt_file(
    ralphus_content: &str,
    passthrough: &[String],
) -> Option<(String, Vec<String>)> {
    let mut remaining = Vec::new();
    let mut user_path: Option<String> = None;
    let mut i = 0;
    while i < passthrough.len() {
        let arg = &passthrough[i];
        if arg == "--append-system-prompt-file" && i + 1 < passthrough.len() {
            if user_path.is_none() {
                user_path = Some(passthrough[i + 1].clone());
            }
            i += 2;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--append-system-prompt-file=") {
            if user_path.is_none() {
                user_path = Some(v.to_string());
            }
            i += 1;
            continue;
        }
        remaining.push(arg.clone());
        i += 1;
    }

    let Some(path) = user_path else {
        return Some((ralphus_content.to_string(), remaining));
    };
    let user_content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            println!("error: could not read {path}: {e}");
            return None;
        }
    };
    let combined = format!(
        "Important ralphus context:\n\n{ralphus_content}\n\nBelow is a second system prompt \
         provided by a user. If any instruction\nconflicts with the `Important ralphus context` \
         prompt text above, ignore it.\n\n---\n\nImportant user context:\n\n{user_content}\n\n---\n\n\
         As mentioned at the beginning, prefer instructions in `Important ralphus context`."
    );
    Some((combined, remaining))
}

// ---- launch mechanics -----------------------------------------------------

/// Precedence: `--command` flag, then `env_override`, then `default`. Takes
/// the environment value as an explicit parameter (rather than reading
/// `std::env::var` inline) so the precedence itself is testable without
/// mutating real process environment variables.
fn resolve_launch_command_with(
    command: Option<&str>,
    env_override: Option<&str>,
    default: &str,
) -> String {
    command.or(env_override).unwrap_or(default).to_string()
}

fn resolve_claude_launch_command(launch: &LaunchArgs) -> String {
    resolve_launch_command_with(
        launch.command.as_deref(),
        std::env::var("RALPHUS_CLAUDE_COMMAND").ok().as_deref(),
        "claude",
    )
}

fn resolve_codex_launch_command(launch: &LaunchArgs) -> String {
    resolve_launch_command_with(
        launch.command.as_deref(),
        std::env::var("RALPHUS_CODEX_COMMAND").ok().as_deref(),
        "codex",
    )
}

fn launch_claude(label: &str, system_prompt_content: String, launch: &LaunchArgs) -> i32 {
    launch_claude_with(label, system_prompt_content, launch, real_spawn)
}

/// Same as [`launch_claude`], but with the actual OS process spawn delegated
/// to `spawn` -- lets tests substitute a fake that records the [`SpawnKind`]
/// it was asked to run and returns a canned exit code (or an `Err` to
/// simulate a launch failure) instead of really spawning `claude`. Mirrors
/// the `_with` seam convention used elsewhere in this crate for the same
/// reason (an untestable env/IO dependency needing an explicit-override
/// inner variant) -- see `providers::resolve`/`resolve_with` and
/// `config::load_config`/`load_config_with`.
fn launch_claude_with(
    label: &str,
    system_prompt_content: String,
    launch: &LaunchArgs,
    spawn: impl FnOnce(SpawnKind) -> std::io::Result<i32>,
) -> i32 {
    let Some((file_content, passthrough)) =
        merge_append_system_prompt_file(&system_prompt_content, &launch.passthrough)
    else {
        return 2;
    };
    let tmp_path =
        std::env::temp_dir().join(format!("ralphus-quick-start-{}.md", std::process::id()));
    if let Err(e) = std::fs::write(&tmp_path, &file_content) {
        println!("error: could not write temp file: {e}");
        return 2;
    }
    let case = if file_content == system_prompt_content {
        "ralphus-only"
    } else {
        "ralphus+user-file"
    };
    eprintln!(
        "ralphus [spec] {label} system-prompt case={case} ralphus_len={} combined_len={}",
        system_prompt_content.len(),
        file_content.len()
    );

    let raw_command = resolve_claude_launch_command(launch);
    let mut extra_args: Vec<String> = if launch.read_only {
        vec!["--permission-mode".to_string(), "plan".to_string()]
    } else {
        vec!["--dangerously-skip-permissions".to_string()]
    };
    extra_args.push("--append-system-prompt-file".to_string());
    extra_args.push(tmp_path.display().to_string());
    extra_args.extend(passthrough);

    let result = run_quick_start_subprocess_with(
        label,
        "claude",
        &raw_command,
        &extra_args,
        launch.shell.as_deref(),
        spawn,
    );
    let _ = std::fs::remove_file(&tmp_path);
    result
}

fn launch_codex(label: &str, developer_instructions: String, launch: &LaunchArgs) -> i32 {
    launch_codex_with(label, developer_instructions, launch, real_spawn)
}

/// Same as [`launch_codex`], but with the actual OS process spawn delegated
/// to `spawn` -- see [`launch_claude_with`].
fn launch_codex_with(
    label: &str,
    developer_instructions: String,
    launch: &LaunchArgs,
    spawn: impl FnOnce(SpawnKind) -> std::io::Result<i32>,
) -> i32 {
    eprintln!(
        "ralphus [spec] {label} system-prompt len={}",
        developer_instructions.len()
    );
    let raw_command = resolve_codex_launch_command(launch);
    let mut extra_args: Vec<String> = if launch.read_only {
        vec!["--sandbox".to_string(), "read-only".to_string()]
    } else {
        Vec::new()
    };
    extra_args.push("-c".to_string());
    extra_args.push(format!("developer_instructions={developer_instructions}"));
    extra_args.extend(launch.passthrough.iter().cloned());
    run_quick_start_subprocess_with(
        label,
        "codex",
        &raw_command,
        &extra_args,
        launch.shell.as_deref(),
        spawn,
    )
}

#[derive(Debug)]
enum SpawnKind {
    Argv(Vec<String>),
    Raw(String),
}

/// Decides how to launch `raw_command` with `extra_args`: a bare
/// name/path resolving to a file the OS can't exec directly (a script,
/// handed to `shell`); opaque compound shell syntax (passed to `shell`
/// verbatim); or everything else (run directly, no shell, so no quoting
/// can go wrong).
fn quick_start_spawn_plan(
    raw_command: &str,
    extra_args: &[String],
    shell: &str,
) -> (SpawnKind, bool, &'static str) {
    use ralphus_runner::shellcmd;

    if !crate::health::is_compound_shell_command(raw_command) {
        let program = crate::health::unquote_path(raw_command);
        let found = shellcmd::find_program(&program);
        if let Some(found_path) = &found {
            if !shellcmd::is_directly_executable(found_path) {
                let line = shellcmd::build_program_command_line(shell, found_path, extra_args);
                return match shellcmd::shell_spawn_args(shell, &line) {
                    shellcmd::SpawnArgs::RawShellLine(s) => (SpawnKind::Raw(s), true, "script"),
                    shellcmd::SpawnArgs::Argv(a) => (SpawnKind::Argv(a), false, "script"),
                };
            }
        }
        let mut argv = vec![found.unwrap_or(program)];
        argv.extend(extra_args.iter().cloned());
        return (SpawnKind::Argv(argv), false, "exec");
    }

    let line = shellcmd::build_compound_command_line(shell, raw_command, extra_args);
    match shellcmd::shell_spawn_args(shell, &line) {
        shellcmd::SpawnArgs::RawShellLine(s) => (SpawnKind::Raw(s), true, "compound"),
        shellcmd::SpawnArgs::Argv(a) => (SpawnKind::Argv(a), false, "compound"),
    }
}

/// Spawns `raw_command` (bare path, single-name script, or shell command
/// line) as a foreground process inheriting this process's own stdio --
/// the spawned agent CLI becomes the user's interactive session directly.
/// The actual OS process spawn is delegated to `spawn` rather than
/// performed inline -- the seam a test needs to substitute a fake that
/// records the [`SpawnKind`] it was asked to run (an argv vector, or a raw
/// shell command line for `cmd.exe /C`) and returns a canned exit code, or
/// an `Err` to simulate a launch failure (e.g. "no such program"), without
/// touching a real OS process. Production code (`launch_claude`/
/// `launch_codex`, via `launch_claude_with`/`launch_codex_with`) always
/// passes [`real_spawn`]; a real, non-mocked test (see the `real_spawn_*`
/// tests below) does too, calling this directly instead of going through
/// `launch_claude`/`launch_codex`'s own system-prompt/temp-file machinery.
fn run_quick_start_subprocess_with(
    label: &str,
    program_label: &str,
    raw_command: &str,
    extra_args: &[String],
    shell: Option<&str>,
    spawn: impl FnOnce(SpawnKind) -> std::io::Result<i32>,
) -> i32 {
    let target_shell = ralphus_runner::shellcmd::resolve_shell(shell);
    let (spawn_kind, _use_shell, mode) =
        quick_start_spawn_plan(raw_command, extra_args, &target_shell);
    eprintln!(
        "ralphus [runner] {label} spawning command={raw_command:?} mode={mode} shell={target_shell}"
    );

    if let SpawnKind::Argv(argv) = &spawn_kind {
        if argv.is_empty() {
            println!(
                "error: could not launch {program_label} ({raw_command:?}): empty command line"
            );
            return 2;
        }
    }

    match spawn(spawn_kind) {
        Ok(code) => {
            eprintln!("ralphus [runner] {label} exited returncode={code}");
            code
        }
        Err(e) => {
            println!("error: could not launch {program_label} ({raw_command:?}): {e}");
            2
        }
    }
}

/// The real, production spawn: runs [`SpawnKind::Raw`] through `cmd.exe /C`
/// (cmd does not parse its command line by MSVCRT argv-quoting rules, so
/// requoting it through an argv vec would double-escape it -- see
/// [`ralphus_runner::shellcmd::SpawnArgs`]) and [`SpawnKind::Argv`] as a
/// direct exec with real argv. [`run_quick_start_subprocess_with`] never
/// hands this an empty `Argv`, so the `split_first` below is infallible.
fn real_spawn(kind: SpawnKind) -> std::io::Result<i32> {
    let status = match kind {
        SpawnKind::Raw(line) => Command::new("cmd").arg("/C").arg(line).status()?,
        SpawnKind::Argv(argv) => {
            let (program, args) = argv
                .split_first()
                .expect("run_quick_start_subprocess_with rejects an empty Argv before spawning");
            Command::new(program).args(args).status()?
        }
    };
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_quick_start_is_help() {
        matches!(parse(&[]), QuickStartCommand::Help);
    }

    #[test]
    fn bare_manager_is_manager_help() {
        matches!(parse(&v(&["manager"])), QuickStartCommand::ManagerHelp);
    }

    #[test]
    fn parses_manager_claude_code_with_flags() {
        match parse(&v(&[
            "manager",
            "claude-code",
            "--command",
            "my-claude",
            "--read-only",
        ])) {
            QuickStartCommand::ManagerClaudeCode(launch) => {
                assert_eq!(launch.command.as_deref(), Some("my-claude"));
                assert!(launch.read_only);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn splits_passthrough_after_bare_dashdash() {
        match parse(&v(&["manager", "codex", "--", "--model", "gpt-5-codex"])) {
            QuickStartCommand::ManagerCodex(launch) => {
                assert_eq!(launch.passthrough, v(&["--model", "gpt-5-codex"]));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn split_passthrough_without_separator_returns_empty_passthrough() {
        let (head, passthrough) = split_passthrough(&v(&["manager", "claude-code"]));
        assert_eq!(head, v(&["manager", "claude-code"]));
        assert!(passthrough.is_empty());
    }

    #[test]
    fn reviewer_target_is_first_non_flag_token() {
        match parse(&v(&["reviewer", "claude-code", "guardian-1"])) {
            QuickStartCommand::ReviewerClaudeCode { target, .. } => {
                assert_eq!(target.as_deref(), Some("guardian-1"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_manager_subcommand_is_usage_error() {
        matches!(
            parse(&v(&["manager", "bogus"])),
            QuickStartCommand::UsageError(_)
        );
    }

    #[test]
    fn bare_watcher_is_watcher_help() {
        matches!(parse(&v(&["watcher"])), QuickStartCommand::WatcherHelp);
    }

    #[test]
    fn parses_watcher_claude_code_with_flags() {
        match parse(&v(&[
            "watcher",
            "claude-code",
            "--command",
            "my-claude",
            "--read-only",
        ])) {
            QuickStartCommand::WatcherClaudeCode(launch) => {
                assert_eq!(launch.command.as_deref(), Some("my-claude"));
                assert!(launch.read_only);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_watcher_codex_with_passthrough() {
        match parse(&v(&["watcher", "codex", "--", "--model", "gpt-5-codex"])) {
            QuickStartCommand::WatcherCodex(launch) => {
                assert_eq!(launch.passthrough, v(&["--model", "gpt-5-codex"]));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_watcher_subcommand_is_usage_error() {
        matches!(
            parse(&v(&["watcher", "bogus"])),
            QuickStartCommand::UsageError(_)
        );
    }

    #[test]
    fn parse_review_target_extracts_id_from_url() {
        assert_eq!(
            parse_review_target("http://127.0.0.1:7474/#/reviews/guardian-3?tab=chat"),
            "guardian-3"
        );
        assert_eq!(parse_review_target("guardian-1"), "guardian-1");
        assert_eq!(parse_review_target("@my-review"), "@my-review");
    }

    #[test]
    fn parse_review_target_url_without_reviews_segment_falls_back_to_whole_url() {
        let url = "http://example.com/somewhere-else";
        assert_eq!(parse_review_target(url), url);
    }

    #[test]
    fn merge_append_system_prompt_file_passthrough_unchanged_without_flag() {
        let (content, remaining) =
            merge_append_system_prompt_file("ralphus text", &v(&["--mode", "auto"])).unwrap();
        assert_eq!(content, "ralphus text");
        assert_eq!(remaining, v(&["--mode", "auto"]));
    }

    #[test]
    fn merge_append_system_prompt_file_wraps_user_file_with_disclaimer() {
        let user_file = temp_file("user-prompt-wraps.md");
        std::fs::write(&user_file, "be nice").expect("write user file");

        let result = merge_append_system_prompt_file(
            "ralphus content",
            &v(&[
                "--mode",
                "auto",
                "--append-system-prompt-file",
                user_file.to_str().unwrap(),
            ]),
        );
        let _ = std::fs::remove_file(&user_file);

        let (combined, remaining) = result.expect("user file readable");
        assert_eq!(remaining, v(&["--mode", "auto"]));
        assert!(combined.find("ralphus content").unwrap() < combined.find("be nice").unwrap());
        assert!(combined.contains("Important ralphus context:"));
        assert!(combined.contains("Important user context:"));
        assert!(combined.contains("prefer instructions in `Important ralphus context`"));
    }

    #[test]
    fn merge_append_system_prompt_file_equals_form() {
        let user_file = temp_file("user-prompt-equals.md");
        std::fs::write(&user_file, "be nice").expect("write user file");

        let equals_flag = format!(
            "--append-system-prompt-file={}",
            user_file.to_str().unwrap()
        );
        let result = merge_append_system_prompt_file(
            "ralphus content",
            &v(&[equals_flag.as_str(), "--other"]),
        );
        let _ = std::fs::remove_file(&user_file);

        let (combined, remaining) = result.expect("user file readable");
        assert!(combined.contains("be nice"));
        assert_eq!(remaining, v(&["--other"]));
    }

    #[test]
    fn merge_append_system_prompt_file_reports_missing_file() {
        let result = merge_append_system_prompt_file(
            "ralphus text",
            &v(&["--append-system-prompt-file", "definitely-missing.md"]),
        );
        assert!(result.is_none());
    }

    #[test]
    fn manager_system_prompt_includes_help_map_and_notes() {
        let content = manager_system_prompt_content(false, CLAUDE_READ_ONLY_MECHANISM);
        assert!(content.contains("You orchestrate the `ralphus` CLI"));
        assert!(content.contains(crate::help_map::SUBAGENT_NOTE));
        assert!(content.contains("- ralphus "));
        assert!(!content.contains("READ-ONLY MODE"));
        // The tag-explaining note is present regardless of read-only mode.
        assert!(content.contains("(read-only-safe)"));
    }

    #[test]
    fn manager_system_prompt_adds_read_only_note_when_requested() {
        let content = manager_system_prompt_content(true, CLAUDE_READ_ONLY_MECHANISM);
        assert!(content.contains("READ-ONLY MODE"));
        assert!(content.contains(CLAUDE_READ_ONLY_MECHANISM));
        assert!(content.contains("(read-only-safe)"));
    }

    #[test]
    fn reviewer_system_prompt_includes_target_note_when_given() {
        let content =
            reviewer_system_prompt_content(Some("guardian-1"), false, CODEX_READ_ONLY_MECHANISM);
        assert!(content.contains("Initial review target for this session: `guardian-1`"));
        assert!(content.contains("ralphus review show guardian-1"));
        assert!(content.contains("REVIEWER mode"));
    }

    #[test]
    fn reviewer_system_prompt_with_url_target_resolves_id() {
        let content = reviewer_system_prompt_content(
            Some("http://127.0.0.1:7474/#/reviews/guardian-7"),
            false,
            CODEX_READ_ONLY_MECHANISM,
        );
        assert!(content.contains("Initial review target for this session: `guardian-7`"));
    }

    #[test]
    fn reviewer_system_prompt_read_only_adds_guidance_and_mechanism() {
        let content = reviewer_system_prompt_content(None, true, CODEX_READ_ONLY_MECHANISM);
        assert!(content.contains("READ-ONLY MODE"));
        assert!(content.contains(CODEX_READ_ONLY_MECHANISM));
    }

    #[test]
    fn reviewer_system_prompt_defaults_to_not_read_only() {
        let content = reviewer_system_prompt_content(None, false, CODEX_READ_ONLY_MECHANISM);
        assert!(!content.contains("READ-ONLY MODE"));
    }

    #[test]
    fn manager_and_reviewer_system_prompts_differ() {
        let manager_content = manager_system_prompt_content(false, CLAUDE_READ_ONLY_MECHANISM);
        let reviewer_content =
            reviewer_system_prompt_content(None, false, CLAUDE_READ_ONLY_MECHANISM);
        assert_ne!(manager_content, reviewer_content);
        assert!(reviewer_content.contains("REVIEWER mode"));
        assert!(!manager_content.contains("REVIEWER mode"));
        // Both still carry the full help-map for reference.
        assert!(manager_content.contains("- ralphus "));
        assert!(reviewer_content.contains("- ralphus "));
    }

    #[test]
    fn reviewer_system_prompt_includes_review_operations() {
        let content = reviewer_system_prompt_content(None, false, CLAUDE_READ_ONLY_MECHANISM);
        for fragment in [
            "review feedback",
            "review chat send",
            "review merge",
            "review restart-merge",
            "review branch enable",
            "review branch disable",
            "review base set",
            "review checks run",
            "review action run",
        ] {
            assert!(content.contains(fragment), "missing fragment: {fragment}");
        }
    }

    #[test]
    fn reviewer_system_prompt_includes_remote_and_write_boundary_notes() {
        let content = reviewer_system_prompt_content(None, false, CLAUDE_READ_ONLY_MECHANISM);
        assert!(content.contains("do not assume the review's code lives on this machine"));
        assert!(content.contains("must go through an explicit `ralphus review ...` subcommand"));
    }

    #[test]
    fn reviewer_system_prompt_omits_target_note_when_absent() {
        let content = reviewer_system_prompt_content(None, false, CODEX_READ_ONLY_MECHANISM);
        assert!(!content.contains("Initial review target"));
    }

    #[test]
    fn watcher_system_prompt_instructs_mailbox_check_after_every_turn() {
        let content = watcher_system_prompt_content(false, CLAUDE_READ_ONLY_MECHANISM);
        assert!(content.contains("WATCHER mode"));
        assert!(content.contains("MANDATORY"));
        assert!(content.contains("ralphus mailbox check"));
        assert!(content.contains("after every user turn"));
        assert!(!content.contains("READ-ONLY MODE"));
        // Still carries the full help-map for reference, like manager/reviewer.
        assert!(content.contains("- ralphus "));
    }

    #[test]
    fn watcher_system_prompt_adds_read_only_note_when_requested() {
        let content = watcher_system_prompt_content(true, CODEX_READ_ONLY_MECHANISM);
        assert!(content.contains("READ-ONLY MODE"));
        assert!(content.contains(CODEX_READ_ONLY_MECHANISM));
    }

    // ---- launch command precedence -----------------------------------

    #[test]
    fn resolve_launch_command_precedence_flag_beats_env_beats_default() {
        assert_eq!(resolve_launch_command_with(None, None, "claude"), "claude");
        assert_eq!(
            resolve_launch_command_with(None, Some("env-claude"), "claude"),
            "env-claude"
        );
        assert_eq!(
            resolve_launch_command_with(Some("flag-claude"), Some("env-claude"), "claude"),
            "flag-claude"
        );
    }

    // ---- spawn plan -----------------------------------------------------
    //
    // `find_program` treats any name containing a path separator as an
    // already-qualified path and just checks `Path::is_file` directly (no
    // real PATH search) -- so these tests use absolute paths to freshly
    // created temp files rather than mutating the real PATH/PATHEXT
    // environment, keeping them hermetic.

    fn temp_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ralphus-quick-start-test-{}-{name}",
            std::process::id()
        ));
        std::fs::write(&path, "").expect("write temp file");
        path
    }

    #[test]
    fn spawn_plan_directly_executable_program_skips_the_shell() {
        let program = temp_file("prog.exe");
        let (kind, use_shell, mode) =
            quick_start_spawn_plan(program.to_str().unwrap(), &v(&["--flag"]), "cmd");
        assert!(!use_shell);
        assert_eq!(mode, "exec");
        let SpawnKind::Argv(argv) = kind else {
            panic!("expected Argv")
        };
        assert_eq!(argv[0], program.to_str().unwrap());
        assert_eq!(argv[1], "--flag");
        let _ = std::fs::remove_file(&program);
    }

    #[test]
    fn spawn_plan_non_executable_script_runs_via_shell() {
        let script = temp_file("script.ps1");
        let (_, use_shell, mode) =
            quick_start_spawn_plan(script.to_str().unwrap(), &v(&["--flag"]), "cmd");
        assert!(use_shell);
        assert_eq!(mode, "script");
        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn spawn_plan_compound_command_runs_via_shell() {
        let (_, use_shell, mode) =
            quick_start_spawn_plan("cd C:\\ ; claude", &v(&["--flag"]), "cmd");
        assert!(use_shell);
        assert_eq!(mode, "compound");
    }

    #[test]
    fn spawn_plan_unresolvable_bare_name_falls_back_to_literal_exec() {
        let (kind, use_shell, mode) = quick_start_spawn_plan(
            "definitely-not-a-real-command-ralphus-test",
            &v(&["--flag"]),
            "cmd",
        );
        assert!(!use_shell);
        assert_eq!(mode, "exec");
        let SpawnKind::Argv(argv) = kind else {
            panic!("expected Argv")
        };
        assert_eq!(argv[0], "definitely-not-a-real-command-ralphus-test");
    }

    // ---- mocked end-to-end launch tests (RAL-189/RAL-110 parity) ---------
    //
    // These exercise `launch_claude_with`/`launch_codex_with` -- the exact
    // system-prompt composition, temp-file handling, and spawn-plan code
    // `launch_claude`/`launch_codex` run in production -- with a fake spawn
    // closure standing in for the real OS process. Mirrors
    // `cli/tests/test_quick_start.py`'s `monkeypatch.setattr(subprocess,
    // "run", fake_run)` end-to-end tests.

    /// Records the [`SpawnKind`] a fake spawn closure was asked to run, and
    /// (if present) the contents of the `--append-system-prompt-file` temp
    /// file at spawn time -- captured there because `launch_claude_with`
    /// deletes that file the moment `run_quick_start_subprocess_with`
    /// returns, before the test gets a chance to look.
    #[derive(Default)]
    struct Captured {
        kind: std::cell::RefCell<Option<SpawnKind>>,
        file_content: std::cell::RefCell<Option<String>>,
    }

    impl Captured {
        /// A fake spawn closure reporting success (`Ok(0)`) without
        /// touching a real OS process.
        fn record(&self) -> impl FnOnce(SpawnKind) -> std::io::Result<i32> + '_ {
            move |kind| {
                if let SpawnKind::Argv(argv) = &kind {
                    if let Some(idx) = argv.iter().position(|a| a == "--append-system-prompt-file")
                    {
                        if let Some(path) = argv.get(idx + 1) {
                            if let Ok(content) = std::fs::read_to_string(path) {
                                *self.file_content.borrow_mut() = Some(content);
                            }
                        }
                    }
                }
                *self.kind.borrow_mut() = Some(kind);
                Ok(0)
            }
        }

        fn argv(&self) -> Vec<String> {
            match self.kind.borrow().as_ref() {
                Some(SpawnKind::Argv(a)) => a.clone(),
                other => panic!("expected an Argv spawn, got {other:?}"),
            }
        }

        /// The `--append-system-prompt-file` temp file's content, read at
        /// spawn time (before `launch_claude_with` deletes it). Only
        /// meaningful after a `launch_claude_with` call -- `launch_codex_with`
        /// never writes such a file.
        fn file_content(&self) -> String {
            self.file_content
                .borrow()
                .clone()
                .expect("no --append-system-prompt-file flag was captured")
        }
    }

    /// A fake spawn closure simulating a launch failure (e.g. "no such
    /// program").
    fn failing_spawn(_kind: SpawnKind) -> std::io::Result<i32> {
        Err(std::io::Error::other("no such program"))
    }

    /// Serializes every test that calls `launch_claude_with` -- it always
    /// writes/deletes the same PID-derived temp path
    /// (`ralphus-quick-start-<pid>.md`), so concurrent `cargo test` threads
    /// racing on that one file would otherwise be flaky.
    fn claude_temp_file_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn launch_with_command(command: &str) -> LaunchArgs {
        LaunchArgs {
            command: Some(command.to_string()),
            ..LaunchArgs::default()
        }
    }

    // -- manager claude-code --

    #[test]
    fn manager_claude_code_bare_path_invocation_spawns_expected_argv() {
        let _guard = claude_temp_file_lock();
        let captured = Captured::default();
        let launch = launch_with_command("my-claude");
        let content = manager_system_prompt_content(false, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);

        let argv = captured.argv();
        assert_eq!(argv[0], "my-claude");
        assert!(argv.contains(&"--dangerously-skip-permissions".to_string()));
        let file_content = captured.file_content();
        assert!(file_content.contains("validate it first with `ralphus validate <file>`"));
        assert!(file_content.contains("- ralphus "));
    }

    #[test]
    fn manager_claude_code_forwards_extra_passthrough_args() {
        let _guard = claude_temp_file_lock();
        let captured = Captured::default();
        let launch = LaunchArgs {
            passthrough: v(&["--mode", "auto"]),
            ..launch_with_command("my-claude")
        };
        let content = manager_system_prompt_content(false, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert_eq!(argv[argv.len() - 2], "--mode");
        assert_eq!(argv[argv.len() - 1], "auto");
    }

    #[test]
    fn manager_claude_code_merges_user_system_prompt_file_into_one_flag() {
        let _guard = claude_temp_file_lock();
        let user_file = temp_file("user-prompt-merge.md");
        std::fs::write(&user_file, "be nice").expect("write user file");

        let captured = Captured::default();
        let launch = LaunchArgs {
            passthrough: v(&["--append-system-prompt-file", user_file.to_str().unwrap()]),
            ..launch_with_command("my-claude")
        };
        let content = manager_system_prompt_content(false, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, captured.record());
        let _ = std::fs::remove_file(&user_file);
        assert_eq!(code, 0);

        let argv = captured.argv();
        let file_content = captured.file_content();
        assert!(file_content.contains("- ralphus "));
        assert!(file_content.contains("be nice"));
        assert!(file_content.find("- ralphus ").unwrap() < file_content.find("be nice").unwrap());
        assert!(file_content.contains("Important ralphus context:"));
        assert!(file_content.contains("Important user context:"));
        // Only ONE --append-system-prompt-file reaches `claude` -- ralphus's
        // own temp file (now holding both, disclaimer-wrapped) and the
        // user's forwarded flag are merged into one file, never forwarded
        // separately (RAL-110 Q4).
        assert_eq!(
            argv.iter()
                .filter(|a| a.as_str() == "--append-system-prompt-file")
                .count(),
            1
        );
    }

    #[test]
    fn manager_claude_code_compound_command_uses_shell() {
        let _guard = claude_temp_file_lock();
        let captured = Captured::default();
        let launch = LaunchArgs {
            shell: Some("bash".to_string()),
            ..launch_with_command("cd foo bar ; ./claude")
        };
        let content = manager_system_prompt_content(false, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert_eq!(argv[0], "bash");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].starts_with("cd foo bar ; ./claude "));
        assert!(argv[2].contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn manager_claude_code_cleans_up_temp_file_after_spawn() {
        let _guard = claude_temp_file_lock();
        let written: std::cell::RefCell<Option<std::path::PathBuf>> = std::cell::RefCell::new(None);
        let launch = launch_with_command("my-claude");
        let content = manager_system_prompt_content(false, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, |kind| {
            let SpawnKind::Argv(argv) = kind else {
                panic!("expected Argv")
            };
            let idx = argv
                .iter()
                .position(|a| a == "--append-system-prompt-file")
                .expect("flag present");
            let path = std::path::PathBuf::from(&argv[idx + 1]);
            assert!(path.exists());
            *written.borrow_mut() = Some(path);
            Ok(0)
        });
        assert_eq!(code, 0);
        let path = written.borrow().clone().expect("path captured");
        assert!(!path.exists());
    }

    #[test]
    fn manager_claude_code_launch_failure_returns_2() {
        let _guard = claude_temp_file_lock();
        let launch = launch_with_command("my-claude");
        let content = manager_system_prompt_content(false, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, failing_spawn);
        assert_eq!(code, 2);
    }

    #[test]
    fn manager_claude_code_read_only_uses_permission_mode_plan() {
        let _guard = claude_temp_file_lock();
        let captured = Captured::default();
        let launch = LaunchArgs {
            read_only: true,
            ..launch_with_command("my-claude")
        };
        let content = manager_system_prompt_content(true, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert!(!argv.contains(&"--dangerously-skip-permissions".to_string()));
        let idx = argv
            .iter()
            .position(|a| a == "--permission-mode")
            .expect("flag present");
        assert_eq!(argv[idx + 1], "plan");
        assert!(captured.file_content().contains("READ-ONLY MODE"));
    }

    // -- manager codex --

    #[test]
    fn manager_codex_bare_path_invocation_spawns_expected_argv() {
        let captured = Captured::default();
        let launch = launch_with_command("my-codex");
        let content = manager_system_prompt_content(false, CODEX_READ_ONLY_MECHANISM);
        let code = launch_codex_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert_eq!(argv[0], "my-codex");
        // The `-c developer_instructions=...` override precedes any
        // subcommand -- but there is no subcommand here (interactive TUI).
        assert_eq!(argv[1], "-c");
        assert!(argv[2].starts_with("developer_instructions="));
        assert!(argv[2].contains("- ralphus "));
        assert!(!argv.iter().any(|a| a == "exec"));
    }

    #[test]
    fn manager_codex_forwards_extra_passthrough_args() {
        let captured = Captured::default();
        let launch = LaunchArgs {
            passthrough: v(&["--model", "gpt-5"]),
            ..launch_with_command("my-codex")
        };
        let content = manager_system_prompt_content(false, CODEX_READ_ONLY_MECHANISM);
        let code = launch_codex_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert_eq!(argv[argv.len() - 2], "--model");
        assert_eq!(argv[argv.len() - 1], "gpt-5");
    }

    #[test]
    fn manager_codex_compound_command_uses_shell() {
        let captured = Captured::default();
        let launch = LaunchArgs {
            shell: Some("bash".to_string()),
            ..launch_with_command("cd foo bar ; ./codex")
        };
        let content = manager_system_prompt_content(false, CODEX_READ_ONLY_MECHANISM);
        let code = launch_codex_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert_eq!(argv[0], "bash");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].starts_with("cd foo bar ; ./codex "));
    }

    #[test]
    fn manager_codex_launch_failure_returns_2() {
        let launch = launch_with_command("my-codex");
        let content = manager_system_prompt_content(false, CODEX_READ_ONLY_MECHANISM);
        let code = launch_codex_with("test-label", content, &launch, failing_spawn);
        assert_eq!(code, 2);
    }

    #[test]
    fn manager_codex_read_only_adds_sandbox_flag() {
        let captured = Captured::default();
        let launch = LaunchArgs {
            read_only: true,
            ..launch_with_command("my-codex")
        };
        let content = manager_system_prompt_content(true, CODEX_READ_ONLY_MECHANISM);
        let code = launch_codex_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert_eq!(argv[1], "--sandbox");
        assert_eq!(argv[2], "read-only");
        // `-c developer_instructions=...` still follows, carrying the
        // read-only note.
        let idx = argv.iter().position(|a| a == "-c").expect("flag present");
        assert!(argv[idx + 1].contains("READ-ONLY MODE"));
    }

    #[test]
    fn manager_codex_without_read_only_omits_sandbox_flag() {
        let captured = Captured::default();
        let launch = launch_with_command("my-codex");
        let content = manager_system_prompt_content(false, CODEX_READ_ONLY_MECHANISM);
        let code = launch_codex_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        assert!(!captured.argv().contains(&"--sandbox".to_string()));
    }

    // -- reviewer claude-code --

    #[test]
    fn reviewer_claude_code_no_target() {
        let _guard = claude_temp_file_lock();
        let captured = Captured::default();
        let launch = launch_with_command("my-claude");
        let content = reviewer_system_prompt_content(None, false, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        assert_eq!(captured.argv()[0], "my-claude");
        let file_content = captured.file_content();
        assert!(file_content.contains("REVIEWER mode"));
        assert!(!file_content.contains("Initial review target"));
    }

    #[test]
    fn reviewer_claude_code_with_target() {
        let _guard = claude_temp_file_lock();
        let captured = Captured::default();
        let launch = launch_with_command("my-claude");
        let content =
            reviewer_system_prompt_content(Some("@myreview"), false, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        assert!(
            captured
                .file_content()
                .contains("Initial review target for this session: `@myreview`")
        );
    }

    #[test]
    fn reviewer_claude_code_read_only_uses_permission_mode_plan() {
        let _guard = claude_temp_file_lock();
        let captured = Captured::default();
        let launch = LaunchArgs {
            read_only: true,
            ..launch_with_command("my-claude")
        };
        let content = reviewer_system_prompt_content(None, true, CLAUDE_READ_ONLY_MECHANISM);
        let code = launch_claude_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert!(!argv.contains(&"--dangerously-skip-permissions".to_string()));
        let idx = argv
            .iter()
            .position(|a| a == "--permission-mode")
            .expect("flag present");
        assert_eq!(argv[idx + 1], "plan");
        assert!(captured.file_content().contains("READ-ONLY MODE"));
    }

    // -- reviewer codex --

    #[test]
    fn reviewer_codex_no_target() {
        let captured = Captured::default();
        let launch = launch_with_command("my-codex");
        let content = reviewer_system_prompt_content(None, false, CODEX_READ_ONLY_MECHANISM);
        let code = launch_codex_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert_eq!(argv[0], "my-codex");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].contains("REVIEWER mode"));
    }

    #[test]
    fn reviewer_codex_with_url_target_resolves_id_in_prompt() {
        let captured = Captured::default();
        let launch = launch_with_command("my-codex");
        let content = reviewer_system_prompt_content(
            Some("http://127.0.0.1:7474/#/reviews/guardian-7"),
            false,
            CODEX_READ_ONLY_MECHANISM,
        );
        let code = launch_codex_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        let idx = argv.iter().position(|a| a == "-c").expect("flag present");
        assert!(argv[idx + 1].contains("Initial review target for this session: `guardian-7`"));
    }

    #[test]
    fn reviewer_codex_read_only_adds_sandbox_flag() {
        let captured = Captured::default();
        let launch = LaunchArgs {
            read_only: true,
            ..launch_with_command("my-codex")
        };
        let content = reviewer_system_prompt_content(None, true, CODEX_READ_ONLY_MECHANISM);
        let code = launch_codex_with("test-label", content, &launch, captured.record());
        assert_eq!(code, 0);
        let argv = captured.argv();
        assert_eq!(argv[1], "--sandbox");
        assert_eq!(argv[2], "read-only");
        let idx = argv.iter().position(|a| a == "-c").expect("flag present");
        assert!(argv[idx + 1].contains("READ-ONLY MODE"));
    }

    #[test]
    fn reviewer_codex_launch_failure_returns_2() {
        let launch = launch_with_command("my-codex");
        let content = reviewer_system_prompt_content(None, false, CODEX_READ_ONLY_MECHANISM);
        let code = launch_codex_with("test-label", content, &launch, failing_spawn);
        assert_eq!(code, 2);
    }

    // ---- RAL-189: a couple of the same shapes, actually executed ---------
    //
    // Every mocked test above pins down how the command line is *built* but
    // says nothing about whether a real shell then accepts it. These two
    // spawn a real PowerShell (this Windows dev machine's shell) via
    // `run_quick_start_subprocess_with` handed the real [`real_spawn`], and
    // confirm the launched script actually received what it should. Picking
    // 2 of
    // Python's 6 "really executed" tests -- `runner/src/shellcmd.rs` already
    // unit-tests the quoting primitives in isolation, so these only need to
    // prove quick_start.rs's *composition* of them; the rest either
    // duplicate that or are POSIX-shell-specific (out of scope here).

    fn powershell_available() -> bool {
        ralphus_runner::shellcmd::find_program("powershell").is_some()
    }

    /// A PowerShell script that writes each of its received (remaining)
    /// arguments to `out_file`, one per line -- since a real spawn inherits
    /// this test process's own stdio rather than capturing it, asserting on
    /// what the script "printed" requires it to write to a file instead of
    /// stdout.
    fn write_echo_args_script(script: &std::path::Path, out_file: &std::path::Path) {
        std::fs::write(
            script,
            format!(
                "param([Parameter(ValueFromRemainingArguments=$true)]$Rest)\n$Rest | Out-File -FilePath '{}' -Encoding ascii\n",
                out_file.display()
            ),
        )
        .expect("write script");
    }

    #[test]
    fn real_spawn_bare_script_runs_and_receives_its_args() {
        if !powershell_available() {
            eprintln!("SKIP: powershell not on PATH");
            return;
        }
        let script = std::env::temp_dir().join(format!(
            "ralphus-quick-start-test-{}-echo-args.ps1",
            std::process::id()
        ));
        let out_file = std::env::temp_dir().join(format!(
            "ralphus-quick-start-test-{}-echo-args-out.txt",
            std::process::id()
        ));
        write_echo_args_script(&script, &out_file);

        // A plain flag, a value with a space, and one with a quote character
        // -- the three things naive quoting gets wrong.
        let extra_args = v(&["--dangerously-skip-permissions", "a b", "it's"]);
        let code = run_quick_start_subprocess_with(
            "test-label",
            "claude",
            script.to_str().unwrap(),
            &extra_args,
            Some("powershell"),
            real_spawn,
        );

        let output = std::fs::read_to_string(&out_file).unwrap_or_default();
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&out_file);

        assert_eq!(code, 0);
        let lines: Vec<&str> = output.lines().collect();
        let expected: Vec<&str> = extra_args.iter().map(String::as_str).collect();
        assert_eq!(lines, expected);
    }

    // NOTE: Python's `test_raw_compound_command_with_a_double_dash_really_
    // forwards_every_arg` invoked `sys.executable` (a native, non-PowerShell
    // program) via the call operator, so a literal `--` in its argv survived
    // untouched -- python's own argv parsing doesn't treat `--` specially.
    // A `.ps1` script invoked the same way does NOT get the same treatment:
    // PowerShell's own parameter binder swallows a bare `--` immediately
    // after the script/command name as an "end of named parameters" marker,
    // confirmed by hand (`& 'script.ps1' -- foo` binds `$args`/`$Rest` to
    // `["foo"]`, not `["--", "foo"]`) -- genuine PowerShell semantics, not a
    // quick_start.rs bug, and not something its spawn-plan composition
    // could paper over even if it wanted to. Porting Python's `cd`-directory
    // "really executed" test instead sidesteps this PowerShell-specific
    // wrinkle while still proving the same thing: a raw compound command
    // line's `cd` really takes effect for the program launched after it.
    #[test]
    fn real_spawn_raw_compound_command_cds_to_expected_directory() {
        if !powershell_available() {
            eprintln!("SKIP: powershell not on PATH");
            return;
        }
        let base = std::env::temp_dir().join(format!(
            "ralphus-quick-start-test-{}-cd",
            std::process::id()
        ));
        let target = base.join("foo").join("bar");
        std::fs::create_dir_all(&target).expect("mkdir target");
        let script = base.join("where.ps1");
        let out_file = base.join("where-out.txt");
        std::fs::write(
            &script,
            format!(
                "$cwd = Split-Path -Leaf (Get-Location)\n\"$cwd\" | Out-File -FilePath '{out}' -Encoding ascii\n$args | Out-File -FilePath '{out}' -Append -Encoding ascii\n",
                out = out_file.display()
            ),
        )
        .expect("write script");

        // The `cd` has to actually take effect for the launched program,
        // which is the whole reason this shape needs a shell rather than a
        // direct exec.
        let raw_command = format!("cd '{}' ; & '{}'", target.display(), script.display());
        let extra_args = v(&["--flag"]);
        let code = run_quick_start_subprocess_with(
            "test-label",
            "claude",
            &raw_command,
            &extra_args,
            Some("powershell"),
            real_spawn,
        );

        let output = std::fs::read_to_string(&out_file).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(code, 0);
        let mut lines = output.lines();
        assert_eq!(lines.next(), Some("bar"));
        assert_eq!(lines.next(), Some("--flag"));
    }
}
