//! `ralphus project <subcommand>`, ported from `cli/src/ralphus/__main__.py`'s
//! `project` group (git/list/get). Bare `ralphus project` prints group help
//! (`p_project.set_defaults(func=_help_printer(p_project))` in the Python
//! source), unlike `queue`/`machine`/`agent`, which default to `list`.
//!
//! Note: none of these three Python handlers call `emit`/honor `--json` --
//! they always print plain human-readable text and use their own literal
//! exit codes on a daemon error (1 for `git`, 2 for `list`/`get`), rather
//! than the `run_and_report`/`exit_code_for` convention used elsewhere in
//! this CLI. This is a faithful 1:1 port of that (mildly inconsistent, but
//! deliberate) Python behavior -- see `cli/src/ralphus/__main__.py`'s
//! `_cmd_project_git`/`_cmd_project_list`/`_cmd_project_get`.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::commands::CommandError;
use crate::flags::{Scanner, UsageError};

const SHORT_DESCRIPTION_MAX: usize = 80;

#[derive(Debug, Clone)]
pub enum ProjectCommand {
    Help,
    Git {
        path: String,
        clone_url: Option<String>,
        /// RAL-355: explicitly clear a previously registered clone URL.
        /// Mutually exclusive with `clone_url` -- `parse_git` rejects both
        /// being set at once rather than picking a silent precedence.
        clear_url: bool,
        name: String,
        description: String,
        /// RAL-307: explicit per-project default for whether a newly
        /// submitted PR defaults to the worktree/feature branch name.
        /// `None` stamps the live global config's value instead.
        match_pr_branch_name: Option<bool>,
    },
    List {
        short: bool,
    },
    Get {
        name: String,
    },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> ProjectCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => ProjectCommand::Help,
        Some("git") => match parse_git(&mut scanner) {
            Ok(cmd) => cmd,
            Err(e) => ProjectCommand::UsageError(e.0),
        },
        Some("list") => {
            let short = scanner.take_bool("--short");
            ProjectCommand::List { short }
        }
        Some("get") => match scanner.remaining().into_iter().next() {
            Some(name) => ProjectCommand::Get { name },
            None => ProjectCommand::UsageError("get requires a <name> argument".to_string()),
        },
        Some(other) => ProjectCommand::UsageError(format!("unknown project subcommand: {other}")),
    }
}

fn parse_git(scanner: &mut Scanner) -> Result<ProjectCommand, UsageError> {
    let path = scanner.take_value("--path")?;
    let name = scanner.take_value("--name")?;
    let clone_url = scanner.take_value("--url")?;
    let clear_url = scanner.take_bool("--clear-url");
    let description = scanner.take_value("--description")?.unwrap_or_default();
    let match_pr_branch_name =
        crate::commands::review::take_tri_bool(scanner, "--match-pr-branch-name");
    let Some(path) = path else {
        return Err(UsageError("project git requires --path".to_string()));
    };
    let Some(name) = name else {
        return Err(UsageError("project git requires --name".to_string()));
    };
    if clear_url && clone_url.is_some() {
        return Err(UsageError(
            "project git: --clear-url cannot be combined with --url".to_string(),
        ));
    }
    Ok(ProjectCommand::Git {
        path,
        clone_url,
        clear_url,
        name,
        description,
        match_pr_branch_name,
    })
}

#[must_use]
pub fn dispatch(cmd: ProjectCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        ProjectCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["project"]).expect("project help exists")
            );
            0
        }
        ProjectCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ProjectCommand::Git {
            path,
            clone_url,
            clear_url,
            name,
            description,
            match_pr_branch_name,
        } => {
            // A leading `~` is expanded (see `ralphus_core::expand_home`); no
            // other relative-path expansion is done, matching this crate's
            // `cmd_initialize_git` in misc.rs.
            let target = ralphus_core::expand_home(&path);
            let target =
                ralphus_core::strip_verbatim_prefix(target.canonicalize().unwrap_or(target));
            let target_str = target.to_string_lossy().to_string();
            match client.register_project(
                &name,
                &target_str,
                &description,
                "git",
                clone_url.as_deref(),
                clear_url,
                match_pr_branch_name,
            ) {
                Ok(payload) => {
                    println!("registered project \"{name}\" -> {target_str}");
                    for warning in payload["warnings"].as_array().into_iter().flatten() {
                        if let Some(warning) = warning.as_str() {
                            println!("warning: {warning}");
                        }
                    }
                    0
                }
                Err(e) => {
                    CommandError::Daemon(e).print(false, None);
                    1
                }
            }
        }
        ProjectCommand::List { short } => match client.list_projects() {
            Ok(payload) => {
                render_project_list(&payload, short);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
        ProjectCommand::Get { name } => match client.get_project(&name) {
            Ok(p) => {
                render_project_detail(&p);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                2
            }
        },
    }
}

/// Truncates `text` to `max_len` chars, replacing the tail with "..." if cut
/// -- ports Python's `_elide_right`.
fn elide_right(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_len.saturating_sub(3)).collect();
    format!("{truncated}...")
}

fn render_project_list(payload: &Value, short: bool) {
    let projects = payload["projects"].as_array().cloned().unwrap_or_default();
    if projects.is_empty() {
        println!("no registered projects");
        return;
    }
    for p in &projects {
        println!(
            "{:<20}  {:<4}  {}",
            p["name"].as_str().unwrap_or_default(),
            p["vcs"].as_str().unwrap_or_default(),
            p["path"].as_str().unwrap_or_default()
        );
        println!(
            "    url: {}",
            p["clone_url"].as_str().unwrap_or("(not registered)")
        );
        if let Some(description) = p["description"].as_str().filter(|d| !d.is_empty()) {
            let description = if short {
                elide_right(description, SHORT_DESCRIPTION_MAX)
            } else {
                description.to_string()
            };
            println!("    {description}");
        }
    }
}

fn render_project_detail(p: &Value) {
    println!("name:        {}", p["name"].as_str().unwrap_or_default());
    println!("vcs:         {}", p["vcs"].as_str().unwrap_or_default());
    println!("path:        {}", p["path"].as_str().unwrap_or_default());
    println!(
        "clone URL:   {}",
        p["clone_url"].as_str().unwrap_or("(not registered)")
    );
    println!(
        "description: {}",
        p["description"].as_str().unwrap_or_default()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_project_is_help() {
        matches!(parse(&[]), ProjectCommand::Help);
    }

    #[test]
    fn parses_git_with_required_flags() {
        match parse(&v(&[
            "git",
            "--path",
            "/repo",
            "--name",
            "my-project",
            "--description",
            "desc",
        ])) {
            ProjectCommand::Git {
                path,
                name,
                description,
                clone_url,
                clear_url,
                match_pr_branch_name,
            } => {
                assert_eq!(path, "/repo");
                assert_eq!(name, "my-project");
                assert_eq!(description, "desc");
                assert_eq!(clone_url, None);
                assert!(!clear_url);
                assert_eq!(match_pr_branch_name, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_git_match_pr_branch_name_tri_state() {
        match parse(&v(&[
            "git",
            "--path",
            "/repo",
            "--name",
            "my-project",
            "--match-pr-branch-name",
        ])) {
            ProjectCommand::Git {
                match_pr_branch_name,
                ..
            } => assert_eq!(match_pr_branch_name, Some(true)),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&[
            "git",
            "--path",
            "/repo",
            "--name",
            "my-project",
            "--no-match-pr-branch-name",
        ])) {
            ProjectCommand::Git {
                match_pr_branch_name,
                ..
            } => assert_eq!(match_pr_branch_name, Some(false)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn git_description_defaults_to_empty() {
        match parse(&v(&["git", "--path", "/repo", "--name", "my-project"])) {
            ProjectCommand::Git { description, .. } => assert_eq!(description, ""),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_git_clone_url() {
        match parse(&v(&[
            "git",
            "--path",
            "/repo",
            "--name",
            "my-project",
            "--url",
            "git@example.invalid:team/repo.git",
        ])) {
            ProjectCommand::Git { clone_url, .. } => assert_eq!(
                clone_url.as_deref(),
                Some("git@example.invalid:team/repo.git")
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_git_clear_url() {
        match parse(&v(&[
            "git",
            "--path",
            "/repo",
            "--name",
            "my-project",
            "--clear-url",
        ])) {
            ProjectCommand::Git {
                clone_url,
                clear_url,
                ..
            } => {
                assert_eq!(clone_url, None);
                assert!(clear_url);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn git_clear_url_combined_with_url_is_a_usage_error() {
        assert!(matches!(
            parse(&v(&[
                "git",
                "--path",
                "/repo",
                "--name",
                "my-project",
                "--url",
                "git@example.invalid:team/repo.git",
                "--clear-url",
            ])),
            ProjectCommand::UsageError(_)
        ));
    }

    #[test]
    fn git_requires_path_and_name() {
        matches!(
            parse(&v(&["git", "--path", "/repo"])),
            ProjectCommand::UsageError(_)
        );
        matches!(
            parse(&v(&["git", "--name", "x"])),
            ProjectCommand::UsageError(_)
        );
    }

    #[test]
    fn parses_list_short_flag() {
        match parse(&v(&["list", "--short"])) {
            ProjectCommand::List { short } => assert!(short),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_get_name() {
        match parse(&v(&["get", "my-project"])) {
            ProjectCommand::Get { name } => assert_eq!(name, "my-project"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_requires_name() {
        matches!(parse(&v(&["get"])), ProjectCommand::UsageError(_));
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        matches!(parse(&v(&["bogus"])), ProjectCommand::UsageError(_));
    }

    #[test]
    fn elide_right_truncates_long_text() {
        assert_eq!(elide_right("hello world", 8), "hello...");
        assert_eq!(elide_right("short", 80), "short");
    }
}
