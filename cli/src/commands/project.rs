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
    Fork(ProjectForkCommand),
    UsageError(String),
}

/// `ralphus project fork <subcommand>` (RAL-338): per-project, per-user fork
/// registration. `user: None` means "the project-wide default row"
/// throughout -- distinct from `Some(String::new())`, which this CLI never
/// produces (an empty `--user` value is treated the same as omitting it).
#[derive(Debug, Clone)]
pub enum ProjectForkCommand {
    Help,
    Add {
        project: String,
        url: String,
        user: Option<String>,
        remote_name: Option<String>,
        owner: Option<String>,
    },
    List {
        project: Option<String>,
        user: Option<String>,
        short: bool,
    },
    Set {
        project: String,
        user: Option<String>,
        url: Option<String>,
        remote_name: Option<String>,
        owner: Option<String>,
    },
    Remove {
        project: String,
        user: Option<String>,
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
        Some("fork") => ProjectCommand::Fork(parse_fork(&scanner.remaining())),
        Some(other) => ProjectCommand::UsageError(format!("unknown project subcommand: {other}")),
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

fn parse_fork(args: &[String]) -> ProjectForkCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => ProjectForkCommand::Help,
        Some("add") => match parse_fork_add(&mut scanner) {
            Ok(cmd) => cmd,
            Err(e) => ProjectForkCommand::UsageError(e.0),
        },
        Some("list") => match parse_fork_list(&mut scanner) {
            Ok(cmd) => cmd,
            Err(e) => ProjectForkCommand::UsageError(e.0),
        },
        Some("set") => match parse_fork_set(&mut scanner) {
            Ok(cmd) => cmd,
            Err(e) => ProjectForkCommand::UsageError(e.0),
        },
        Some("remove") => match parse_fork_remove(&mut scanner) {
            Ok(cmd) => cmd,
            Err(e) => ProjectForkCommand::UsageError(e.0),
        },
        Some(other) => {
            ProjectForkCommand::UsageError(format!("unknown project fork subcommand: {other}"))
        }
    }
}

fn parse_fork_add(scanner: &mut Scanner) -> Result<ProjectForkCommand, UsageError> {
    let url = scanner.take_value("--url")?;
    let user = non_empty(scanner.take_value("--user")?);
    let remote_name = non_empty(scanner.take_value("--remote-name")?);
    let owner = non_empty(scanner.take_value("--owner")?);
    let Some(project) = scanner.clone().remaining().into_iter().next() else {
        return Err(UsageError(
            "project fork add requires a <project> argument".to_string(),
        ));
    };
    let Some(url) = url else {
        return Err(UsageError("project fork add requires --url".to_string()));
    };
    Ok(ProjectForkCommand::Add {
        project,
        url,
        user,
        remote_name,
        owner,
    })
}

fn parse_fork_list(scanner: &mut Scanner) -> Result<ProjectForkCommand, UsageError> {
    let user = non_empty(scanner.take_value("--user")?);
    let short = scanner.take_bool("--short");
    let project = scanner.clone().remaining().into_iter().next();
    Ok(ProjectForkCommand::List {
        project,
        user,
        short,
    })
}

fn parse_fork_set(scanner: &mut Scanner) -> Result<ProjectForkCommand, UsageError> {
    let user = non_empty(scanner.take_value("--user")?);
    let url = scanner.take_value("--url")?;
    let remote_name = scanner.take_value("--remote-name")?;
    let owner = scanner.take_value("--owner")?;
    let Some(project) = scanner.clone().remaining().into_iter().next() else {
        return Err(UsageError(
            "project fork set requires a <project> argument".to_string(),
        ));
    };
    Ok(ProjectForkCommand::Set {
        project,
        user,
        url,
        remote_name,
        owner,
    })
}

fn parse_fork_remove(scanner: &mut Scanner) -> Result<ProjectForkCommand, UsageError> {
    let user = non_empty(scanner.take_value("--user")?);
    let Some(project) = scanner.clone().remaining().into_iter().next() else {
        return Err(UsageError(
            "project fork remove requires a <project> argument".to_string(),
        ));
    };
    Ok(ProjectForkCommand::Remove { project, user })
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
        ProjectCommand::Fork(cmd) => dispatch_fork(cmd, opts),
    }
}

#[must_use]
fn dispatch_fork(cmd: ProjectForkCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        ProjectForkCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["project", "fork"])
                    .expect("project fork help exists")
            );
            0
        }
        ProjectForkCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        ProjectForkCommand::Add {
            project,
            url,
            user,
            remote_name,
            owner,
        } => match client.add_project_fork(
            &project,
            user.as_deref().unwrap_or(""),
            &url,
            remote_name.as_deref(),
            owner.as_deref(),
        ) {
            Ok(record) => {
                println!(
                    "registered fork for project \"{project}\" user {:?} -> {url}",
                    user.as_deref().unwrap_or("(default)")
                );
                render_fork_detail(&record);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
        ProjectForkCommand::List {
            project,
            user,
            short,
        } => {
            let result = match &project {
                Some(p) => client.list_project_forks(p),
                None => client.list_all_project_forks(),
            };
            match result {
                Ok(payload) => {
                    render_fork_list(&payload, user.as_deref(), short);
                    0
                }
                Err(e) => {
                    CommandError::Daemon(e).print(false, None);
                    2
                }
            }
        }
        ProjectForkCommand::Set {
            project,
            user,
            url,
            remote_name,
            owner,
        } => match client.set_project_fork(
            &project,
            user.as_deref().unwrap_or(""),
            url.as_deref(),
            remote_name.as_deref(),
            owner.as_deref(),
        ) {
            Ok(record) => {
                println!("updated fork for project \"{project}\"");
                render_fork_detail(&record);
                0
            }
            Err(e) => {
                CommandError::Daemon(e).print(false, None);
                1
            }
        },
        ProjectForkCommand::Remove { project, user } => {
            match client.remove_project_fork(&project, user.as_deref().unwrap_or("")) {
                Ok(_) => {
                    println!(
                        "removed fork for project \"{project}\" user {:?}",
                        user.as_deref().unwrap_or("(default)")
                    );
                    0
                }
                Err(e) => {
                    CommandError::Daemon(e).print(false, None);
                    1
                }
            }
        }
    }
}

fn render_fork_detail(f: &Value) {
    println!("project:     {}", f["project"].as_str().unwrap_or_default());
    let user = f["user"].as_str().unwrap_or_default();
    println!(
        "user:        {}",
        if user.is_empty() { "(default)" } else { user }
    );
    println!(
        "fork_url:    {}",
        f["fork_url"].as_str().unwrap_or_default()
    );
    println!(
        "remote_name: {}",
        f["remote_name"].as_str().unwrap_or_default()
    );
    let owner = f["fork_owner"].as_str().unwrap_or_default();
    if !owner.is_empty() {
        println!("fork_owner:  {owner}");
    }
}

fn render_fork_list(payload: &Value, user_filter: Option<&str>, short: bool) {
    let forks: Vec<&Value> = payload["forks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| user_filter.is_none_or(|u| f["user"].as_str().unwrap_or_default() == u))
        .collect();
    if forks.is_empty() {
        println!("no registered forks");
        return;
    }
    for f in forks {
        let user = f["user"].as_str().unwrap_or_default();
        let user_display = if user.is_empty() { "(default)" } else { user };
        if short {
            println!(
                "{:<20}  {:<12}  {}",
                f["project"].as_str().unwrap_or_default(),
                user_display,
                f["fork_url"].as_str().unwrap_or_default()
            );
        } else {
            println!(
                "{:<20}  {:<12}  {}",
                f["project"].as_str().unwrap_or_default(),
                user_display,
                f["fork_url"].as_str().unwrap_or_default()
            );
            println!(
                "    remote: {}   owner: {}",
                f["remote_name"].as_str().unwrap_or_default(),
                f["fork_owner"].as_str().unwrap_or("")
            );
        }
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

    #[test]
    fn parses_fork_add_with_required_flags() {
        match parse(&v(&[
            "fork",
            "add",
            "proj",
            "--url",
            "git@x:alice/proj.git",
        ])) {
            ProjectCommand::Fork(ProjectForkCommand::Add {
                project,
                url,
                user,
                remote_name,
                owner,
            }) => {
                assert_eq!(project, "proj");
                assert_eq!(url, "git@x:alice/proj.git");
                assert_eq!(user, None);
                assert_eq!(remote_name, None);
                assert_eq!(owner, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_fork_add_with_all_flags() {
        match parse(&v(&[
            "fork",
            "add",
            "proj",
            "--url",
            "git@x:alice/proj.git",
            "--user",
            "alice",
            "--remote-name",
            "fork-alice",
            "--owner",
            "alice",
        ])) {
            ProjectCommand::Fork(ProjectForkCommand::Add {
                project,
                url,
                user,
                remote_name,
                owner,
            }) => {
                assert_eq!(project, "proj");
                assert_eq!(url, "git@x:alice/proj.git");
                assert_eq!(user.as_deref(), Some("alice"));
                assert_eq!(remote_name.as_deref(), Some("fork-alice"));
                assert_eq!(owner.as_deref(), Some("alice"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn fork_add_requires_project_and_url() {
        assert!(matches!(
            parse(&v(&["fork", "add", "--url", "url"])),
            ProjectCommand::Fork(ProjectForkCommand::UsageError(_))
        ));
        assert!(matches!(
            parse(&v(&["fork", "add", "proj"])),
            ProjectCommand::Fork(ProjectForkCommand::UsageError(_))
        ));
    }

    #[test]
    fn parses_fork_list_with_optional_project_and_user() {
        match parse(&v(&["fork", "list"])) {
            ProjectCommand::Fork(ProjectForkCommand::List {
                project,
                user,
                short,
            }) => {
                assert_eq!(project, None);
                assert_eq!(user, None);
                assert!(!short);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["fork", "list", "proj", "--user", "alice", "--short"])) {
            ProjectCommand::Fork(ProjectForkCommand::List {
                project,
                user,
                short,
            }) => {
                assert_eq!(project.as_deref(), Some("proj"));
                assert_eq!(user.as_deref(), Some("alice"));
                assert!(short);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_fork_set_fields() {
        match parse(&v(&[
            "fork", "set", "proj", "--user", "alice", "--url", "new-url",
        ])) {
            ProjectCommand::Fork(ProjectForkCommand::Set {
                project,
                user,
                url,
                remote_name,
                owner,
            }) => {
                assert_eq!(project, "proj");
                assert_eq!(user.as_deref(), Some("alice"));
                assert_eq!(url.as_deref(), Some("new-url"));
                assert_eq!(remote_name, None);
                assert_eq!(owner, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn fork_set_requires_project() {
        assert!(matches!(
            parse(&v(&["fork", "set", "--url", "url"])),
            ProjectCommand::Fork(ProjectForkCommand::UsageError(_))
        ));
    }

    #[test]
    fn parses_fork_remove() {
        match parse(&v(&["fork", "remove", "proj", "--user", "alice"])) {
            ProjectCommand::Fork(ProjectForkCommand::Remove { project, user }) => {
                assert_eq!(project, "proj");
                assert_eq!(user.as_deref(), Some("alice"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&["fork", "remove", "proj"])) {
            ProjectCommand::Fork(ProjectForkCommand::Remove { project, user }) => {
                assert_eq!(project, "proj");
                assert_eq!(user, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn fork_remove_requires_project() {
        assert!(matches!(
            parse(&v(&["fork", "remove"])),
            ProjectCommand::Fork(ProjectForkCommand::UsageError(_))
        ));
    }

    #[test]
    fn bare_fork_and_unknown_fork_subcommand() {
        assert!(matches!(
            parse(&v(&["fork"])),
            ProjectCommand::Fork(ProjectForkCommand::Help)
        ));
        assert!(matches!(
            parse(&v(&["fork", "bogus"])),
            ProjectCommand::Fork(ProjectForkCommand::UsageError(_))
        ));
    }
}
