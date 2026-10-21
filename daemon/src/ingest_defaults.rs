//! Fallback defaults for ingesting existing branches: which base branch the
//! ingested review stacks on and which agent its cell runs.
//!
//! Both are resolved here, never guessed inside [`crate::worktrees::default_branch`],
//! which stays a no-guessing lookup (RAL-100). Every resolution reports the
//! rule that produced it so the ingest output can say why.

use crate::config::ReviewConfig;
use std::path::Path;

/// Agent used for an ingested cell when no config layer names one.
pub const FALLBACK_AGENT: &str = "claude-code";
/// Model used with [`FALLBACK_AGENT`] when no config layer names one.
pub const FALLBACK_MODEL: &str = "haiku";

/// Branch names the closest-common-branch rule considers, in tie-break order.
/// `staging` is deliberately absent: it is never a base to guess.
const COMMON_BASES: [&str; 3] = ["main", "master", "beta"];

/// A resolved base branch and the rule that chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBase {
    pub branch: String,
    pub rule: String,
}

/// A resolved agent/model and the rule that chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub agent: String,
    pub model: Option<String>,
    pub rule: String,
    /// True when no layer set an agent and the Haiku fallback applied.
    pub fell_back: bool,
}

/// Resolve the base branch for ingesting `branches` in the repository at `root`.
///
/// Order: `explicit` (`--base`), then the base of the base-most open PR among
/// `branches` (`pr_base` maps a branch to its open PR's base), then the
/// `default_base_branch` setting, then the repository's default branch, then
/// the closest of `main`/`master`/`beta`.
///
/// # Errors
/// PRs with different bases that do not chain, a failing PR lookup, an empty
/// `branches`, or no candidate base at all; each message names the fix.
pub fn resolve_base(
    root: &Path,
    branches: &[String],
    explicit: Option<&str>,
    config: &ReviewConfig,
    pr_base: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<ResolvedBase, String> {
    if let Some(base) = explicit.map(str::trim).filter(|b| !b.is_empty()) {
        return Ok(ResolvedBase {
            branch: base.to_string(),
            rule: "`--base`".to_string(),
        });
    }
    if let Some((base, owner)) = pr_stack_base(branches, pr_base)? {
        return Ok(ResolvedBase {
            rule: format!("base of the open pull request for {owner}"),
            branch: base,
        });
    }
    if let Some(base) = config.default_base_branch() {
        return Ok(ResolvedBase {
            branch: base.to_string(),
            rule: "`[review] default_base_branch` setting".to_string(),
        });
    }
    if let Ok(base) = crate::worktrees::default_branch(root) {
        return Ok(ResolvedBase {
            branch: base,
            rule: "repository default branch (remote HEAD)".to_string(),
        });
    }
    let branch = closest_common_branch(root, branches)?;
    Ok(ResolvedBase {
        rule: "closest common branch; set `[review] default_base_branch` to pin it".to_string(),
        branch,
    })
}

/// The base of the base-most open PR among `branches`, with the branch that
/// owns it. `None` when no branch has an open PR.
///
/// PRs whose bases differ are only acceptable as a chain: a PR whose base is
/// another ingested branch is a link, and exactly one distinct root base may
/// remain.
fn pr_stack_base(
    branches: &[String],
    pr_base: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<Option<(String, String)>, String> {
    let mut roots: Vec<(String, String)> = Vec::new();
    for branch in branches {
        let Some(base) = pr_base(branch)? else {
            continue;
        };
        if branches.contains(&base) || roots.iter().any(|(_, b)| *b == base) {
            continue;
        }
        roots.push((branch.clone(), base));
    }
    match roots.as_slice() {
        [] => Ok(None),
        [(branch, base)] => Ok(Some((base.clone(), format!("`{branch}`")))),
        many => Err(format!(
            "the ingested branches have open pull requests into different bases that do not \
             chain ({}); pass `--base <branch>` to choose one",
            many.iter()
                .map(|(b, base)| format!("`{b}` -> `{base}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The common branch with the fewest commits between its merge-base with the
/// ingested tip and that tip. Local and `origin` copies of a name both count;
/// ties go in [`COMMON_BASES`] order.
fn closest_common_branch(root: &Path, branches: &[String]) -> Result<String, String> {
    let tip = branches
        .first()
        .ok_or_else(|| "no branches to ingest; pass at least one branch".to_string())?;
    let mut best: Option<(u64, &str)> = None;
    for name in COMMON_BASES {
        for candidate in [
            format!("refs/heads/{name}"),
            format!("refs/remotes/origin/{name}"),
        ] {
            let git = |args: &[&str]| crate::guardian_merge::git(root, args);
            if git(&["rev-parse", "--verify", "--quiet", &candidate]).is_err() {
                continue;
            }
            let Ok(base) = git(&["merge-base", &candidate, tip]) else {
                continue;
            };
            let range = format!("{}..{tip}", base.trim());
            let Ok(count) = git(&["rev-list", "--count", &range]) else {
                continue;
            };
            let Ok(count) = count.trim().parse::<u64>() else {
                continue;
            };
            if best.is_none_or(|(c, _)| count < c) {
                best = Some((count, name));
            }
        }
    }
    best.map(|(_, name)| name.to_string()).ok_or_else(|| {
        format!(
            "could not choose a base branch in {}: none of main, master or beta exists \
             (`staging` is never guessed). Fix: set `[review] default_base_branch` in \
             `.ralphus.toml` (or `ralphus project review-settings set --default-base-branch \
             <branch>`), or pass `--base <branch>`",
            root.display()
        )
    })
}

/// Resolve the agent and model for an ingested cell from the review's own
/// defaults, falling back to Claude Code + Haiku when no layer sets an agent.
///
/// # Errors
/// An invalid merged config (e.g. an empty `default_base_branch`).
pub fn resolve_agent(config: &ReviewConfig) -> Result<ResolvedAgent, String> {
    config.validate()?;
    let agent = config
        .default_resolver_agent
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    let model = config
        .default_resolver_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    Ok(match agent {
        Some(agent) => ResolvedAgent {
            agent: agent.to_string(),
            model: model.map(str::to_string),
            rule: "`[review] default_resolver_agent` setting".to_string(),
            fell_back: false,
        },
        None => ResolvedAgent {
            agent: FALLBACK_AGENT.to_string(),
            model: Some(model.unwrap_or(FALLBACK_MODEL).to_string()),
            rule: format!(
                "fallback {FALLBACK_AGENT}/{FALLBACK_MODEL}; set `[review] default_resolver_agent` \
                 and `default_resolver_model` to use another agent"
            ),
            fell_back: true,
        },
    })
}

/// Message for an ingested cell whose agent could not start, naming the
/// setting to fill.
#[must_use]
pub fn agent_unavailable_message(agent: &ResolvedAgent, cause: &str) -> String {
    format!(
        "ingest agent `{}` is unavailable ({cause}). Fix: install and sign in to it, or set \
         `[review] default_resolver_agent` (and `default_resolver_model`) in `.ralphus.toml` or \
         with `ralphus project review-settings set --resolver-agent <agent>`",
        agent.agent
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit(dir: &Path, msg: &str) {
        run(dir, &["commit", "-q", "--allow-empty", "-m", msg]);
    }

    fn repo(tag: &str, init_branch: &str) -> std::path::PathBuf {
        let dir = ralphus_test_support::scratch(format!("ral703-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", init_branch]);
        commit(&dir, "root");
        dir
    }

    fn no_prs(_: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn cfg(toml: &str) -> ReviewConfig {
        crate::config::from_toml_str(toml)
    }

    #[test]
    fn explicit_base_wins() {
        let dir = repo("explicit", "main");
        let r = resolve_base(
            &dir,
            &["f".into()],
            Some("rel"),
            &ReviewConfig::default(),
            &no_prs,
        )
        .unwrap();
        assert_eq!(r.branch, "rel");
    }

    #[test]
    fn setting_used_when_no_pr() {
        let dir = repo("setting", "develop");
        let c = cfg("[review]\ndefault_base_branch = \"develop\"\n");
        let r = resolve_base(&dir, &["develop".into()], None, &c, &no_prs).unwrap();
        assert_eq!(r.branch, "develop");
        assert!(r.rule.contains("default_base_branch"));
    }

    #[test]
    fn open_pr_base_beats_setting() {
        let dir = repo("prbase", "main");
        let c = cfg("[review]\ndefault_base_branch = \"develop\"\n");
        let pr = |b: &str| Ok((b == "feat").then(|| "release/2.0".to_string()));
        let r = resolve_base(&dir, &["feat".into()], None, &c, &pr).unwrap();
        assert_eq!(r.branch, "release/2.0");
    }

    #[test]
    fn chained_prs_resolve_to_the_root_base() {
        let dir = repo("chain", "main");
        let pr = |b: &str| {
            Ok(match b {
                "a" => Some("release".to_string()),
                "b" => Some("a".to_string()),
                _ => None,
            })
        };
        let r = resolve_base(
            &dir,
            &["a".into(), "b".into()],
            None,
            &ReviewConfig::default(),
            &pr,
        )
        .unwrap();
        assert_eq!(r.branch, "release");
    }

    #[test]
    fn unchained_pr_bases_are_refused() {
        let dir = repo("split", "main");
        let pr = |b: &str| Ok(Some(if b == "a" { "x" } else { "y" }.to_string()));
        let err = resolve_base(
            &dir,
            &["a".into(), "b".into()],
            None,
            &ReviewConfig::default(),
            &pr,
        )
        .unwrap_err();
        assert!(err.contains("--base"), "{err}");
    }

    #[test]
    fn tied_main_and_master_pick_main() {
        let dir = repo("tie", "master");
        run(&dir, &["branch", "main"]);
        run(&dir, &["checkout", "-q", "-b", "feat"]);
        commit(&dir, "f1");
        let r = resolve_base(
            &dir,
            &["feat".into()],
            None,
            &ReviewConfig::default(),
            &no_prs,
        )
        .unwrap();
        assert_eq!(r.branch, "main");
        assert!(r.rule.contains("closest common branch"));
    }

    #[test]
    fn nearer_branch_beats_tie_order() {
        let dir = repo("closer", "main");
        run(&dir, &["checkout", "-q", "-b", "master"]);
        commit(&dir, "m1");
        run(&dir, &["checkout", "-q", "-b", "feat"]);
        commit(&dir, "f1");
        let r = resolve_base(
            &dir,
            &["feat".into()],
            None,
            &ReviewConfig::default(),
            &no_prs,
        )
        .unwrap();
        assert_eq!(r.branch, "master");
    }

    #[test]
    fn staging_only_fails_with_remediation() {
        let dir = repo("staging", "staging");
        let err = resolve_base(
            &dir,
            &["staging".into()],
            None,
            &ReviewConfig::default(),
            &no_prs,
        )
        .unwrap_err();
        assert!(
            err.contains("default_base_branch") && err.contains("--base"),
            "{err}"
        );
    }

    #[test]
    fn agent_falls_back_to_haiku() {
        let a = resolve_agent(&ReviewConfig::default()).unwrap();
        assert_eq!(
            (a.agent.as_str(), a.model.as_deref()),
            ("claude-code", Some("haiku"))
        );
        assert!(a.fell_back);
        assert!(agent_unavailable_message(&a, "not signed in").contains("default_resolver_agent"));
    }

    #[test]
    fn configured_agent_is_used() {
        let c =
            cfg("[review]\ndefault_resolver_agent = \"codex\"\ndefault_resolver_model = \"m\"\n");
        let a = resolve_agent(&c).unwrap();
        assert_eq!(
            (a.agent.as_str(), a.model.as_deref(), a.fell_back),
            ("codex", Some("m"), false)
        );
    }

    #[test]
    fn whitespace_base_setting_is_rejected() {
        let c = ReviewConfig {
            default_base_branch: Some("  ".into()),
            ..ReviewConfig::default()
        };
        assert!(resolve_agent(&c).is_err());
    }
}
