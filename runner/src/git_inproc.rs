//! Read-only git questions answered in-process with libgit2, so the callers
//! that ask them dozens of times a minute stop costing an OS process each.
//!
//! Two consumers share this module because the daemon already links this
//! crate as a library: the cell runner's live diff watcher
//! ([`crate::worktree_diff`]) and the daemon's single git spawn funnel
//! (`GitVcs::exec_raw`), which asks [`try_read`] before it spawns anything.
//!
//! Everything here is an optimisation with an exact subprocess equivalent, so
//! every path answers only when it can answer exactly and otherwise reports
//! "ask real git": an `Err` from [`Repo`], or `None` from [`try_read`]. That
//! covers a repository libgit2 cannot open (reftable, foreign owner,
//! a `root` that is not the top of a checkout), history that libgit2 walks
//! differently from git (shallow clones, `refs/replace`, grafts, merges in a
//! `log` range), and any subject line git's `%s` would reformat.
//!
//! Mutations, network operations and anything that runs hooks, credential
//! helpers, rerere or smudge filters stay on real `git`; libgit2 reproduces
//! none of them.
//!
//! `RALPHUS_GIT_INPROC=0` turns every path off and restores the pure
//! subprocess behaviour.

use std::path::Path;
use std::sync::OnceLock;

use git2::{Delta, DiffOptions, Oid, Repository, Sort, Status, StatusOptions, StatusShow};

/// Whether in-process git is enabled for this process (read once).
#[must_use]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("RALPHUS_GIT_INPROC")
                .map(|v| v.trim().to_ascii_lowercase())
                .as_deref(),
            Ok("0" | "false" | "off" | "no")
        )
    })
}

/// What changed in a worktree relative to a baseline commit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Changes {
    /// Tracked files that differ from the baseline (`git diff --numstat` rows).
    pub files_changed: u64,
    /// Of those, files that exist now but not in the baseline (`create mode`).
    pub files_added: u64,
    /// Of those, files that existed in the baseline but not now (`delete mode`).
    pub files_removed: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
    /// Untracked, not-ignored paths relative to the worktree root, in the form
    /// `git ls-files --others --exclude-standard` prints them (a nested
    /// repository is one entry with a trailing `/`).
    pub untracked: Vec<String>,
}

/// An open handle on the checkout whose top directory is the `root` it was
/// opened with. Keep one for the life of a watcher: reopening per poll costs
/// tens of milliseconds on a large repository.
pub struct Repo {
    repo: Repository,
}

impl Repo {
    /// Open the checkout at exactly `root`.
    ///
    /// `Repository::open` does not search parent directories, so a `root`
    /// that is a subdirectory of a checkout is refused here -- git run from
    /// such a directory scopes `ls-files` to the subtree, which this module
    /// does not reproduce.
    ///
    /// # Errors
    /// In-process git is disabled, or libgit2 cannot open `root`.
    pub fn open(root: &Path) -> Result<Self, String> {
        if !enabled() {
            return Err("in-process git is disabled (RALPHUS_GIT_INPROC=0)".to_string());
        }
        let repo = Repository::open(root).map_err(|e| format!("libgit2 open: {e}"))?;
        if repo.is_bare() {
            return Err("bare repository".to_string());
        }
        Ok(Self { repo })
    }

    /// The commit `HEAD` resolves to, as a lowercase hex id.
    ///
    /// # Errors
    /// `HEAD` is unborn or unreadable.
    pub fn head_commit(&self) -> Result<String, String> {
        let head = self.repo.head().map_err(|e| format!("libgit2 head: {e}"))?;
        let commit = head
            .peel_to_commit()
            .map_err(|e| format!("libgit2 head commit: {e}"))?;
        Ok(commit.id().to_string())
    }

    /// Everything changed since `baseline` (a commit id): tracked changes as
    /// `git diff --numstat --summary --no-renames --no-ext-diff
    /// --no-textconv <baseline>` reports them, and untracked files as
    /// `git ls-files --others --exclude-standard` lists them.
    ///
    /// Neither half writes the index (`UPDATE_INDEX` is never set), so it
    /// cannot contend with the agent's own git operations.
    ///
    /// # Errors
    /// `baseline` is not a commit in this repository, or libgit2 failed to
    /// read the index, tree or working directory.
    pub fn changes_since(&self, baseline: &str) -> Result<Changes, String> {
        let oid = Oid::from_str(baseline).map_err(|e| format!("libgit2 baseline id: {e}"))?;
        let tree = self
            .repo
            .find_commit(oid)
            .and_then(|c| c.tree())
            .map_err(|e| format!("libgit2 baseline tree: {e}"))?;

        let mut options = DiffOptions::new();
        let diff = self
            .repo
            .diff_tree_to_workdir_with_index(Some(&tree), Some(&mut options))
            .map_err(|e| format!("libgit2 diff: {e}"))?;
        let stats = diff
            .stats()
            .map_err(|e| format!("libgit2 diff stats: {e}"))?;
        let mut changes = Changes {
            files_changed: stats.files_changed() as u64,
            lines_added: stats.insertions() as u64,
            lines_removed: stats.deletions() as u64,
            ..Changes::default()
        };
        for delta in diff.deltas() {
            match delta.status() {
                Delta::Added => changes.files_added += 1,
                Delta::Deleted => changes.files_removed += 1,
                _ => {}
            }
        }

        let mut status_options = StatusOptions::new();
        status_options
            .show(StatusShow::Workdir)
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false)
            .include_unmodified(false)
            .exclude_submodules(false);
        let statuses = self
            .repo
            .statuses(Some(&mut status_options))
            .map_err(|e| format!("libgit2 status: {e}"))?;
        for entry in statuses.iter() {
            if entry.status().contains(Status::WT_NEW) {
                changes
                    .untracked
                    .push(String::from_utf8_lossy(entry.path_bytes()).into_owned());
            }
        }
        Ok(changes)
    }

    /// Whether history walks match what git would print: libgit2 honours
    /// neither shallow boundaries, `refs/replace` nor grafts the way git does.
    fn history_is_plain(&self) -> bool {
        // A linked worktree's git dir names the shared one in `commondir`.
        let git_dir = self.repo.path();
        let common = std::fs::read_to_string(git_dir.join("commondir"))
            .map_or_else(|_| git_dir.to_path_buf(), |text| git_dir.join(text.trim()));
        !(common.join("shallow").exists()
            || common.join("info").join("grafts").exists()
            || common.join("refs").join("replace").exists()
            || self.repo.path().join("shallow").exists())
    }
}

/// A parsed read this module can answer.
enum Read<'a> {
    /// `log [--reverse] --format=%s <range>`
    Subjects { range: &'a str, reverse: bool },
    /// `rev-list --count <range>`
    Count { range: &'a str },
    /// `ls-tree -r -z --name-only <ref>`
    LsTree { reference: &'a str },
    /// `symbolic-ref [--quiet] [--short] <ref>`
    SymbolicRef { reference: &'a str, short: bool },
    /// `for-each-ref --format=%(refname[:short]) [<pattern>...]`
    ForEachRef {
        short: bool,
        patterns: &'a [&'a str],
    },
}

/// A single plain ref name (or `HEAD`): no revision syntax, no options.
fn plain_name(part: &str) -> bool {
    !part.is_empty()
        && !part.starts_with('-')
        && !part.starts_with('/')
        && !part.ends_with('/')
        && !part.contains("//")
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        && !part.contains("..")
}

/// A range of the shape `A..B` where both ends are plain ref names or hex
/// ids -- no `...`, no revision syntax, no options.
fn plain_range(s: &str) -> bool {
    let plain = |part: &str| {
        !part.is_empty()
            && !part.starts_with('-')
            && !part.starts_with('/')
            && !part.ends_with('/')
            && !part.contains("//")
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
            && !part.contains("..")
    };
    match s.split_once("..") {
        Some((a, b)) => plain(a) && plain(b) && !b.starts_with('.'),
        None => false,
    }
}

fn parse<'a>(args: &'a [&'a str]) -> Option<Read<'a>> {
    match args {
        ["log", "--format=%s", range] if plain_range(range) => Some(Read::Subjects {
            range,
            reverse: false,
        }),
        ["log", "--reverse", "--format=%s", range] if plain_range(range) => Some(Read::Subjects {
            range,
            reverse: true,
        }),
        ["rev-list", "--count", range] if plain_range(range) => Some(Read::Count { range }),
        ["ls-tree", "-r", "-z", "--name-only", reference] if plain_name(reference) => {
            Some(Read::LsTree { reference })
        }
        ["symbolic-ref", rest @ ..] => {
            let mut short = false;
            let mut reference = None;
            for arg in rest {
                match *arg {
                    "--quiet" | "-q" => {}
                    "--short" => short = true,
                    a if plain_name(a) && reference.replace(a).is_none() => {}
                    _ => return None,
                }
            }
            reference.map(|reference| Read::SymbolicRef { reference, short })
        }
        ["for-each-ref", format, patterns @ ..]
            if matches!(*format, "--format=%(refname)" | "--format=%(refname:short)")
                && patterns.iter().all(|p| ref_pattern(p)) =>
        {
            Some(Read::ForEachRef {
                short: *format == "--format=%(refname:short)",
                patterns,
            })
        }
        _ => None,
    }
}

/// A `for-each-ref` pattern this module matches exactly: a literal ref
/// prefix, or one ending in a single `/*`.
fn ref_pattern(p: &str) -> bool {
    let body = p.strip_suffix("/*").unwrap_or(p);
    body.starts_with("refs/") && plain_name(body) && !body.contains('*')
}

/// One commit's `%s`, or `None` when git might print something different:
/// a message that is not UTF-8, declares another encoding, starts with blank
/// lines, or whose first paragraph is not a single clean line.
fn clean_subject(commit: &git2::Commit<'_>) -> Option<String> {
    if commit.message_encoding().is_some() {
        return None;
    }
    let message = std::str::from_utf8(commit.message_bytes()).ok()?;
    let first = message.split('\n').next()?;
    if first.is_empty() || first.trim() != first || first.contains('\r') {
        return None;
    }
    // The paragraph must end after this line: end of message, or a blank line.
    let rest = &message[first.len()..];
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    if rest
        .split('\n')
        .next()
        .is_some_and(|l| !l.trim().is_empty())
    {
        return None;
    }
    Some(first.to_string())
}

/// Answer `git <args>` run in `root` without a process, returning its stdout,
/// when `args` is a read this module reproduces exactly. `None` means "run
/// git". Handles only `log [--reverse] --format=%s A..B` over linear history
/// and `rev-list --count A..B`.
#[must_use]
pub fn try_read(root: &Path, args: &[&str]) -> Option<String> {
    let request = parse(args)?;
    let repo = Repo::open(root).ok()?;
    match request {
        Read::LsTree { reference } => return repo.ls_tree(reference),
        Read::SymbolicRef { reference, short } => return repo.symbolic_ref(reference, short),
        Read::ForEachRef { short, patterns } => return repo.for_each_ref(short, patterns),
        Read::Subjects { .. } | Read::Count { .. } => {}
    }
    if !repo.history_is_plain() {
        return None;
    }
    let (Read::Subjects { range, .. } | Read::Count { range }) = request else {
        return None;
    };
    let mut walk = repo.repo.revwalk().ok()?;
    // Children before parents, newest first among unrelated commits. Time
    // alone breaks ties arbitrarily for commits made in the same second,
    // where git prints the child first; the range is linear, so the
    // topological order is the only valid one.
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME).ok()?;
    walk.push_range(range).ok()?;

    match request {
        Read::Count { .. } => {
            let mut count = 0usize;
            for id in walk {
                id.ok()?;
                count += 1;
            }
            Some(format!("{count}\n"))
        }
        Read::Subjects { reverse, .. } => {
            let mut subjects = Vec::new();
            for id in walk {
                let commit = repo.repo.find_commit(id.ok()?).ok()?;
                if commit.parent_count() > 1 {
                    return None;
                }
                subjects.push(clean_subject(&commit)?);
            }
            if reverse {
                subjects.reverse();
            }
            let mut out = String::new();
            for subject in subjects {
                out.push_str(&subject);
                out.push('\n');
            }
            Some(out)
        }
        _ => None,
    }
}

impl Repo {
    /// `git ls-tree -r -z --name-only <reference>`: every blob and submodule
    /// path in the tree, NUL-terminated, in tree order.
    fn ls_tree(&self, reference: &str) -> Option<String> {
        // `ls-tree` reads through `refs/replace`.
        if !self.history_is_plain() {
            return None;
        }
        let tree = self
            .repo
            .revparse_single(reference)
            .ok()?
            .peel_to_tree()
            .ok()?;
        let mut out = String::new();
        let mut valid = true;
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() == Some(git2::ObjectType::Tree) {
                return git2::TreeWalkResult::Ok;
            }
            match entry.name() {
                Some(name) => {
                    out.push_str(dir);
                    out.push_str(name);
                    out.push('\0');
                    git2::TreeWalkResult::Ok
                }
                None => {
                    valid = false;
                    git2::TreeWalkResult::Abort
                }
            }
        })
        .ok()?;
        valid.then_some(out)
    }

    /// The name `git` prints for `full` with `--short` /
    /// `%(refname:short)`: the shortest form git's own ref-resolution rules
    /// map back to the same ref. `None` when that depends on anything this
    /// module does not model (an ambiguous short name, another namespace, a
    /// remote's `HEAD`).
    fn shorten(&self, full: &str) -> Option<String> {
        // `git`'s `ref_rev_parse_rules`, in order; the match is the first
        // rule `full` fits, and every earlier rule must not name a ref.
        let (prefix_rule, short) = if let Some(s) = full.strip_prefix("refs/tags/") {
            (2, s)
        } else if let Some(s) = full.strip_prefix("refs/heads/") {
            (3, s)
        } else if let Some(s) = full.strip_prefix("refs/remotes/") {
            if s.ends_with("/HEAD") {
                return None;
            }
            (4, s)
        } else {
            return None;
        };
        let earlier = [
            short.to_string(),
            format!("refs/{short}"),
            format!("refs/tags/{short}"),
            format!("refs/heads/{short}"),
        ];
        for candidate in earlier.iter().take(prefix_rule) {
            match self.repo.find_reference(candidate) {
                Ok(_) => return None,
                // libgit2 reports a one-level lowercase name (`main`) as an
                // invalid spec rather than a missing ref.
                Err(e)
                    if matches!(
                        e.code(),
                        git2::ErrorCode::NotFound | git2::ErrorCode::InvalidSpec
                    ) => {}
                Err(_) => return None,
            }
        }
        Some(short.to_string())
    }

    /// `git symbolic-ref [--quiet] [--short] <reference>` for a symbolic ref.
    fn symbolic_ref(&self, reference: &str, short: bool) -> Option<String> {
        let found = self.repo.find_reference(reference).ok()?;
        let target = found.symbolic_target()?.to_string();
        let shown = if short {
            self.shorten(&target)?
        } else {
            target
        };
        Some(format!("{shown}\n"))
    }

    /// `git for-each-ref --format=%(refname[:short]) [<pattern>...]`: refs
    /// sorted by name, filtered to those a pattern matches literally (whole
    /// name, or up to a `/`) or by a trailing `/*`.
    fn for_each_ref(&self, short: bool, patterns: &[&str]) -> Option<String> {
        let mut names = Vec::new();
        for reference in self.repo.references().ok()? {
            let reference = reference.ok()?;
            let name = reference.name()?.to_string();
            // Per-worktree namespaces differ between libgit2 and git.
            if name.starts_with("refs/worktree/") || name.starts_with("refs/bisect/") {
                return None;
            }
            // A ref that does not resolve is skipped by git with a warning.
            reference.resolve().ok()?;
            let wanted = patterns.is_empty()
                || patterns.iter().any(|p| match p.strip_suffix("/*") {
                    Some(prefix) => name.starts_with(&format!("{prefix}/")),
                    None => name == *p || name.starts_with(&format!("{p}/")),
                });
            if wanted {
                names.push(name);
            }
        }
        names.sort();
        let mut out = String::new();
        for name in names {
            out.push_str(&if short { self.shorten(&name)? } else { name });
            out.push('\n');
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    pub(crate) fn run(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub(crate) fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-inproc-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        run(&dir, &["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    fn commit_file(dir: &Path, name: &str, body: &str, message: &str) {
        std::fs::write(dir.join(name), body).unwrap();
        run(dir, &["add", name]);
        run(dir, &["commit", "-q", "-m", message]);
    }

    /// The subprocess answer, for parity.
    fn git_out(dir: &Path, args: &[&str]) -> String {
        run(dir, args)
    }

    #[test]
    fn head_commit_matches_rev_parse() {
        let dir = temp_repo("head");
        let repo = Repo::open(&dir).unwrap();
        assert_eq!(
            repo.head_commit().unwrap(),
            git_out(&dir, &["rev-parse", "--verify", "HEAD"]).trim()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn open_refuses_a_subdirectory_and_a_non_repository() {
        let dir = temp_repo("subdir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(Repo::open(&dir.join("sub")).is_err());
        let plain =
            std::env::temp_dir().join(format!("ralphus-inproc-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        // A temp dir can itself sit inside a repository on a dev machine, but
        // `Repository::open` never searches upward.
        assert!(Repo::open(&plain).is_err());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(plain);
    }

    #[test]
    fn log_subjects_and_rev_list_count_match_git_on_linear_history() {
        let dir = temp_repo("log");
        let base = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        commit_file(&dir, "b.txt", "b\n", "add b");
        commit_file(&dir, "c.txt", "c\n", "add c: with colon");
        commit_file(
            &dir,
            "d.txt",
            "d\n",
            "add d\n\nbody line one\nbody line two",
        );
        let range = format!("{base}..HEAD");

        for (args, expect_some) in [
            (vec!["log", "--format=%s", range.as_str()], true),
            (
                vec!["log", "--reverse", "--format=%s", range.as_str()],
                true,
            ),
            (vec!["rev-list", "--count", range.as_str()], true),
        ] {
            let ours = try_read(&dir, &args);
            assert_eq!(ours.is_some(), expect_some, "{args:?}");
            assert_eq!(ours.unwrap(), git_out(&dir, &args), "{args:?}");
        }
        // An empty range prints nothing / 0 under both.
        let empty = format!("{base}..{base}");
        assert_eq!(
            try_read(&dir, &["log", "--format=%s", &empty]).unwrap(),
            git_out(&dir, &["log", "--format=%s", &empty])
        );
        assert_eq!(
            try_read(&dir, &["rev-list", "--count", &empty]).unwrap(),
            git_out(&dir, &["rev-list", "--count", &empty])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn branch_names_resolve_like_git() {
        let dir = temp_repo("names");
        run(&dir, &["checkout", "-q", "-b", "feature/x"]);
        commit_file(&dir, "f.txt", "f\n", "feature work");
        let args = ["log", "--format=%s", "main..feature/x"];
        assert_eq!(try_read(&dir, &args).unwrap(), git_out(&dir, &args));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn merges_unknown_revisions_and_odd_subjects_fall_back_to_git() {
        let dir = temp_repo("fallback");
        let base = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();

        // Unresolvable revision.
        assert!(try_read(&dir, &["log", "--format=%s", "nope..HEAD"]).is_none());
        assert!(try_read(&dir, &["rev-list", "--count", "nope..HEAD"]).is_none());

        // Unsupported shapes.
        assert!(try_read(&dir, &["log", "--format=%H", &format!("{base}..HEAD")]).is_none());
        assert!(try_read(&dir, &["log", "-1", "--format=%s", "HEAD"]).is_none());
        assert!(try_read(&dir, &["rev-list", "--count", "HEAD"]).is_none());
        assert!(try_read(&dir, &["status"]).is_none());

        // A first paragraph of two lines: git folds it with a space.
        commit_file(&dir, "m.txt", "m\n", "line one\nline two");
        assert!(try_read(&dir, &["log", "--format=%s", &format!("{base}..HEAD")]).is_none());

        // A merge inside the range.
        let dir2 = temp_repo("merge");
        let base2 = git_out(&dir2, &["rev-parse", "HEAD"]).trim().to_string();
        run(&dir2, &["checkout", "-q", "-b", "side"]);
        commit_file(&dir2, "s.txt", "s\n", "side");
        run(&dir2, &["checkout", "-q", "main"]);
        commit_file(&dir2, "t.txt", "t\n", "main");
        run(
            &dir2,
            &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
        );
        let range2 = format!("{base2}..HEAD");
        assert!(try_read(&dir2, &["log", "--format=%s", &range2]).is_none());
        // Counting does not depend on order, so merges are fine there.
        assert_eq!(
            try_read(&dir2, &["rev-list", "--count", &range2]).unwrap(),
            git_out(&dir2, &["rev-list", "--count", &range2])
        );

        // Shallow marker: history differs from git's view.
        std::fs::write(dir2.join(".git").join("shallow"), "").unwrap();
        assert!(try_read(&dir2, &["rev-list", "--count", &range2]).is_none());

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    /// Both answers must be identical, and ours must exist.
    fn same_as_git(dir: &Path, args: &[&str]) {
        let ours = try_read(dir, args).unwrap_or_else(|| panic!("not answered: {args:?}"));
        assert_eq!(ours, git_out(dir, args), "{args:?}");
    }

    #[test]
    fn ls_tree_matches_git() {
        let dir = temp_repo("lstree");
        std::fs::create_dir_all(dir.join("a/b c")).unwrap();
        std::fs::create_dir_all(dir.join("z")).unwrap();
        std::fs::write(dir.join("a/b c/deep file.txt"), "x").unwrap();
        std::fs::write(dir.join("a/top.txt"), "x").unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::write(dir.join("zeta.txt"), "x").unwrap();
        std::fs::write(dir.join("z/ünïcode.txt"), "x").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "tree"]);
        same_as_git(&dir, &["ls-tree", "-r", "-z", "--name-only", "HEAD"]);
        same_as_git(&dir, &["ls-tree", "-r", "-z", "--name-only", "main"]);
        // Unknown ref and unsupported flags go to git.
        assert!(try_read(&dir, &["ls-tree", "-r", "-z", "--name-only", "nope"]).is_none());
        assert!(try_read(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn symbolic_ref_matches_git() {
        let dir = temp_repo("symref");
        run(&dir, &["checkout", "-q", "-b", "feature/x"]);
        same_as_git(&dir, &["symbolic-ref", "HEAD"]);
        same_as_git(&dir, &["symbolic-ref", "--short", "HEAD"]);
        same_as_git(&dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
        // A remote's HEAD symref, full form.
        run(&dir, &["remote", "add", "origin", "file:///nowhere"]);
        run(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run(
            &dir,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        same_as_git(&dir, &["symbolic-ref", "refs/remotes/origin/HEAD"]);
        // Detached HEAD: git's error/exit code is its business.
        run(&dir, &["checkout", "-q", "--detach"]);
        assert!(try_read(&dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_ambiguous_short_branch_name_is_left_to_git() {
        let dir = temp_repo("ambig");
        run(&dir, &["checkout", "-q", "-b", "dup"]);
        run(&dir, &["tag", "dup"]);
        // `refs/tags/dup` shadows `refs/heads/dup`: git prints `heads/dup`.
        assert!(try_read(&dir, &["symbolic-ref", "--short", "HEAD"]).is_none());
        assert!(
            try_read(
                &dir,
                &["for-each-ref", "--format=%(refname:short)", "refs/heads"]
            )
            .is_none()
        );
        // The non-short forms are unaffected.
        same_as_git(&dir, &["symbolic-ref", "HEAD"]);
        same_as_git(&dir, &["for-each-ref", "--format=%(refname)", "refs/heads"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn for_each_ref_matches_git_for_prefixes_globs_and_packed_refs() {
        let dir = temp_repo("foreach");
        for name in [
            "guardian/g1/wt-a",
            "guardian/g1/review",
            "guardian/g12/wt-b",
            "other",
        ] {
            run(&dir, &["branch", name]);
        }
        run(&dir, &["tag", "v1"]);
        run(&dir, &["update-ref", "refs/ralphus/carry/g1/0", "HEAD"]);
        run(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        for pack in [false, true] {
            if pack {
                run(&dir, &["pack-refs", "--all"]);
            }
            same_as_git(&dir, &["for-each-ref", "--format=%(refname)", "refs/heads"]);
            same_as_git(&dir, &["for-each-ref", "--format=%(refname)"]);
            same_as_git(
                &dir,
                &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
            );
            same_as_git(
                &dir,
                &[
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "refs/heads/guardian/g1",
                ],
            );
            same_as_git(
                &dir,
                &[
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "refs/heads/guardian/g1",
                    "refs/heads/guardian/g1/*",
                    "refs/heads/other",
                ],
            );
            same_as_git(
                &dir,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/ralphus/carry/g1",
                ],
            );
            same_as_git(
                &dir,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/heads/nothing-here",
                ],
            );
            same_as_git(
                &dir,
                &["for-each-ref", "--format=%(refname:short)", "refs/remotes"],
            );
            same_as_git(
                &dir,
                &["for-each-ref", "--format=%(refname:short)", "refs/tags"],
            );
        }
        // Unsupported shapes go to git.
        for args in [
            &["for-each-ref", "--format=%(objectname)", "refs/heads"][..],
            &["for-each-ref", "--format=%(refname)", "refs/heads/*/x"],
            &["for-each-ref", "--format=%(refname)", "heads"],
            &["for-each-ref", "--sort=-refname", "--format=%(refname)"],
        ] {
            assert!(try_read(&dir, args).is_none(), "{args:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plain_range_accepts_only_simple_two_dot_ranges() {
        for ok in [
            "a..b",
            "origin/main..HEAD",
            "abc123..def456",
            "x/y-z_1.2..w",
        ] {
            assert!(plain_range(ok), "{ok}");
        }
        for bad in [
            "a...b", "a", "..b", "a..", "-a..b", "a..-b", "a~1..b", "a^..b", "a@{u}..b", "a b..c",
            "a..b..c", "a:b..c",
        ] {
            assert!(!plain_range(bad), "{bad}");
        }
    }

    #[test]
    fn changes_since_reports_tracked_and_untracked_like_git() {
        let dir = temp_repo("changes");
        let base = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        let repo = Repo::open(&dir).unwrap();
        assert_eq!(repo.changes_since(&base).unwrap(), Changes::default());

        // edit, delete, staged-new, committed-after-baseline, binary, untracked, ignored
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        commit_file(&dir, "gone.txt", "g1\ng2\n", "add gone");
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        std::fs::write(dir.join("staged.txt"), "s\n").unwrap();
        run(&dir, &["add", "staged.txt"]);
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 0, 3]).unwrap();
        run(&dir, &["add", "bin.dat"]);
        std::fs::create_dir_all(dir.join("deep/er")).unwrap();
        std::fs::write(dir.join("deep/er/new.txt"), "x\ny\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.join("skip.log"), "ignored\n").unwrap();

        let ours = repo.changes_since(&base).unwrap();
        let theirs = git_changes(&dir, &base);
        assert_eq!(ours.files_changed, theirs.files_changed, "files_changed");
        assert_eq!(ours.files_added, theirs.files_added, "files_added");
        assert_eq!(ours.files_removed, theirs.files_removed, "files_removed");
        assert_eq!(ours.lines_added, theirs.lines_added, "lines_added");
        assert_eq!(ours.lines_removed, theirs.lines_removed, "lines_removed");
        let (mut a, mut b) = (ours.untracked.clone(), theirs.untracked.clone());
        a.sort();
        b.sort();
        assert_eq!(a, b, "untracked");
        assert!(a.iter().any(|p| p == "deep/er/new.txt"));
        assert!(!a.iter().any(|p| p == "skip.log"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_nested_repository_is_one_untracked_entry_like_git() {
        let dir = temp_repo("nested");
        let base = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        run(&inner, &["init", "-q"]);
        std::fs::write(inner.join("f.txt"), "f\n").unwrap();
        let ours = Repo::open(&dir).unwrap().changes_since(&base).unwrap();
        let theirs = git_changes(&dir, &base);
        assert_eq!(ours.untracked, theirs.untracked);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The pre-libgit2 subprocess answer, parsed the way the watcher always did.
    fn git_changes(dir: &Path, baseline: &str) -> Changes {
        let diff = git_out(
            dir,
            &[
                "diff",
                "--numstat",
                "--summary",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                baseline,
            ],
        );
        let others = git_out(dir, &["ls-files", "--others", "--exclude-standard", "-z"]);
        let mut c = Changes::default();
        for line in diff.lines() {
            if let Some(rest) = line.strip_prefix(' ') {
                if rest.starts_with("create mode ") {
                    c.files_added += 1;
                } else if rest.starts_with("delete mode ") {
                    c.files_removed += 1;
                }
                continue;
            }
            let mut parts = line.splitn(3, '\t');
            c.files_changed += 1;
            c.lines_added += parts
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
            c.lines_removed += parts
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
        }
        c.untracked = others
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        c
    }
}
