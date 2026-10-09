//! Answering the commonest `git rev-parse` calls by reading ref files
//! directly, without starting a git process.
//!
//! The review maintenance pass asks "what commit does this ref point at?"
//! dozens of times a minute per review (base branch, every review branch,
//! every PR's private sync ref). Each answer is a single small file read
//! inside the repository, but asking git costs a process each time.
//! [`try_rev_parse`] handles exactly the shapes those callers use --
//! `rev-parse [--verify] [--quiet|-q] <name>[^{commit}]` for a plain ref name
//! -- by reading loose refs, `packed-refs` and `HEAD` with git's own name
//! resolution order, and returns `None` (so the caller runs real git) the
//! moment anything is outside what it can answer exactly: a reftable
//! repository, a name that is not a plain ref (`HEAD~1`, `@{u}`, a SHA, a
//! path), a name more than one ref could mean, a tag, a packed ref with a
//! peeled value, or an unresolvable symbolic ref.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};

/// git's DWIM rules for a short name (`refs.c`'s `ref_rev_parse_rules`), in
/// order; `{}` is the name.
const RULES: &[&str] = &[
    "{}",
    "refs/{}",
    "refs/tags/{}",
    "refs/heads/{}",
    "refs/remotes/{}",
    "refs/remotes/{}/HEAD",
];

/// The parsed shape of a `rev-parse` invocation this module can answer.
struct Request<'a> {
    verify: bool,
    quiet: bool,
    name: &'a str,
}

fn parse<'a>(args: &[&'a str]) -> Option<Request<'a>> {
    let (first, rest) = args.split_first()?;
    if *first != "rev-parse" {
        return None;
    }
    let mut verify = false;
    let mut quiet = false;
    let mut name = None;
    for arg in rest {
        match *arg {
            "--verify" => verify = true,
            "--quiet" | "-q" => quiet = true,
            a if a.starts_with('-') => return None,
            a => {
                if name.replace(a).is_some() {
                    return None;
                }
            }
        }
    }
    let name = name?;
    // A branch, remote or private ref names a commit, so `^{commit}` peels
    // to the same SHA; tags (which may name a tag object) are refused later.
    let name = name.strip_suffix("^{commit}").unwrap_or(name);
    if !is_plain_ref_name(name) {
        return None;
    }
    Some(Request {
        verify,
        quiet,
        name,
    })
}

/// A name made only of characters a branch/remote/ref path uses, with none
/// of the revision syntax (`~ ^ : @{ ..`) and not an object id.
fn is_plain_ref_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        && !(name.len() >= 4 && name.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The repository's per-worktree git dir and its common dir, for a `root`
/// that is the top of a checkout (`root/.git` is a directory or a `gitdir:`
/// file). `None` for anything else, or a reftable repository.
fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let path = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => {
            let path = PathBuf::from(text.trim());
            if path.is_absolute() {
                path
            } else {
                git_dir.join(path)
            }
        }
        Err(_) => git_dir.clone(),
    };
    if common.join("reftable").exists() || git_dir.join("reftable").exists() {
        return None;
    }
    Some((git_dir, common))
}

/// What one full ref name resolves to.
enum Lookup {
    Missing,
    Sha(String),
    /// Present but not answerable here (peeled tag, broken symref, ...).
    Unsure,
}

/// Refs git keeps per worktree rather than in the common dir.
fn is_per_worktree(full: &str) -> bool {
    !full.starts_with("refs/")
        || full.starts_with("refs/bisect/")
        || full.starts_with("refs/worktree/")
        || full.starts_with("refs/rewritten/")
}

fn lookup(git_dir: &Path, common: &Path, full: &str, depth: u8) -> Lookup {
    if depth > 5 {
        return Lookup::Unsure;
    }
    let base = if is_per_worktree(full) {
        git_dir
    } else {
        common
    };
    match std::fs::read_to_string(base.join(full)) {
        Ok(text) => {
            let text = text.trim();
            if let Some(target) = text.strip_prefix("ref:") {
                return match lookup(git_dir, common, target.trim(), depth + 1) {
                    Lookup::Missing => Lookup::Unsure,
                    other => other,
                };
            }
            if is_full_sha(text) {
                Lookup::Sha(text.to_string())
            } else {
                Lookup::Unsure
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !full.starts_with("refs/") {
                return Lookup::Missing;
            }
            packed(common, full)
        }
        Err(_) => Lookup::Unsure,
    }
}

fn packed(common: &Path, full: &str) -> Lookup {
    let text = match std::fs::read_to_string(common.join("packed-refs")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Lookup::Missing,
        Err(_) => return Lookup::Unsure,
    };
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((sha, name)) = line.split_once(' ') else {
            continue;
        };
        if name == full && is_full_sha(sha) {
            // A following `^<sha>` line means the ref points at a tag object.
            if lines.peek().is_some_and(|next| next.starts_with('^')) {
                return Lookup::Unsure;
            }
            return Lookup::Sha(sha.to_string());
        }
    }
    Lookup::Missing
}

fn is_full_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn exit_status(code: u32) -> ExitStatus {
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt as _;
        ExitStatus::from_raw(code)
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        ExitStatus::from_raw(i32::try_from(code).unwrap_or(1) << 8)
    }
}

/// Answer `git <args>` in `root` without a process when `args` is a plain
/// ref `rev-parse` this module can resolve exactly (see the module doc);
/// `None` means "run git".
#[must_use]
pub fn try_rev_parse(root: &Path, args: &[&str]) -> Option<Output> {
    let req = parse(args)?;
    let (git_dir, common) = git_dirs(root)?;
    let candidates: Vec<String> = if req.name.starts_with("refs/") || req.name == "HEAD" {
        vec![req.name.to_string()]
    } else {
        RULES.iter().map(|r| r.replace("{}", req.name)).collect()
    };
    let mut found: Option<(String, String)> = None;
    for full in &candidates {
        match lookup(&git_dir, &common, full, 0) {
            Lookup::Missing => {}
            Lookup::Unsure => return None,
            Lookup::Sha(sha) => {
                if found.is_some() {
                    return None; // ambiguous: let git decide and warn
                }
                found = Some((full.clone(), sha));
            }
        }
    }
    match found {
        Some((full, sha)) => {
            // A tag may point at a tag object, which `^{commit}` (and plain
            // rev-parse of an annotated tag) would peel or report differently.
            if full.starts_with("refs/tags/") {
                return None;
            }
            Some(Output {
                status: exit_status(0),
                stdout: format!("{sha}\n").into_bytes(),
                stderr: Vec::new(),
            })
        }
        // Without `--verify --quiet`, git's failure output (it echoes the
        // name, then errors) is not worth reproducing; let git produce it.
        None if req.verify && req.quiet => Some(Output {
            status: exit_status(1),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git")
    }

    fn repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-vcs-refs-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(git(&dir, &["init", "-q", "-b", "main"]).status.success());
        std::fs::write(dir.join("a.txt"), "a").expect("write");
        assert!(git(&dir, &["add", "a.txt"]).status.success());
        assert!(git(&dir, &["commit", "-q", "-m", "one"]).status.success());
        dir
    }

    /// Every case where this module answers must match real git byte for
    /// byte (stdout and success); a `None` is always acceptable.
    fn agrees(root: &Path, args: &[&str]) -> bool {
        let Some(ours) = try_rev_parse(root, args) else {
            return false;
        };
        let real = git(root, args);
        assert_eq!(
            ours.stdout, real.stdout,
            "stdout differs for {args:?} in {root:?}"
        );
        assert_eq!(
            ours.status.success(),
            real.status.success(),
            "status differs for {args:?}"
        );
        true
    }

    #[test]
    fn loose_packed_remote_symbolic_and_missing_refs_match_git() {
        let root = repo("basic");
        assert!(git(&root, &["branch", "feature"]).status.success());
        assert!(
            git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"])
                .status
                .success()
        );
        assert!(
            git(&root, &["update-ref", "refs/ralphus/sync/pr-1", "HEAD"])
                .status
                .success()
        );

        for args in [
            &["rev-parse", "feature"][..],
            &["rev-parse", "--verify", "feature"],
            &["rev-parse", "main"],
            &["rev-parse", "HEAD"],
            &["rev-parse", "--verify", "refs/remotes/origin/main"],
            &["rev-parse", "--verify", "refs/remotes/origin/main^{commit}"],
            &["rev-parse", "origin/main"],
            &["rev-parse", "refs/ralphus/sync/pr-1"],
            &["rev-parse", "--verify", "--quiet", "refs/ralphus/sync/pr-1"],
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/ralphus/sync/missing",
            ],
            &["rev-parse", "--verify", "-q", "no-such-branch"],
        ] {
            assert!(agrees(&root, args), "expected an answer for {args:?}");
        }

        assert!(git(&root, &["pack-refs", "--all"]).status.success());
        for args in [
            &["rev-parse", "feature"][..],
            &["rev-parse", "--verify", "refs/remotes/origin/main^{commit}"],
            &["rev-parse", "--verify", "--quiet", "refs/ralphus/sync/pr-1"],
        ] {
            assert!(
                agrees(&root, args),
                "expected an answer for packed {args:?}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn linked_worktree_head_and_branches_match_git() {
        let root = repo("wt");
        let wt = root.with_extension("wt");
        assert!(
            git(
                &root,
                &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()]
            )
            .status
            .success()
        );
        for args in [
            &["rev-parse", "HEAD"][..],
            &["rev-parse", "side"],
            &["rev-parse", "--verify", "main"],
        ] {
            assert!(
                agrees(&wt, args),
                "expected an answer in worktree for {args:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn anything_it_cannot_answer_exactly_falls_back_to_git() {
        let root = repo("fallback");
        assert!(
            git(&root, &["tag", "-a", "v1", "-m", "v1"])
                .status
                .success()
        );
        assert!(git(&root, &["branch", "v1-branch"]).status.success());
        // Ambiguous: a branch and a tag both named `dup`.
        assert!(git(&root, &["branch", "dup"]).status.success());
        assert!(git(&root, &["tag", "dup"]).status.success());
        for args in [
            &["rev-parse", "v1"][..],
            &["rev-parse", "dup"],
            &["rev-parse", "HEAD~1"],
            &["rev-parse", "main@{u}"],
            &["rev-parse", "--abbrev-ref", "HEAD"],
            &["rev-parse", "--git-path", "hooks"],
            &["rev-parse", "missing-without-verify"],
            &["rev-parse", "main", "feature"],
            &["log", "-1"],
        ] {
            assert!(
                try_rev_parse(&root, args).is_none(),
                "should fall back to git for {args:?}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn plain_ref_names_exclude_revision_syntax_and_object_ids() {
        assert!(is_plain_ref_name("bench-a-review"));
        assert!(is_plain_ref_name("refs/remotes/origin/main"));
        for name in [
            "HEAD~1", "a..b", "main@{u}", "x:y", "deadbeef", "/abs", "a//b",
        ] {
            assert!(!is_plain_ref_name(name), "{name}");
        }
    }
}
