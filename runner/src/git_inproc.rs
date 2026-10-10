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

/// Whether git config is being injected through the environment
/// (`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`, or the older
/// `GIT_CONFIG_PARAMETERS` that `git -c` sets). That is git's highest-priority
/// config scope and libgit2 does not read it, so every in-process answer that
/// depends on config -- and the layout answer for the hooks directory -- steps
/// aside and lets real git (which does read it) answer.
#[must_use]
pub fn config_env_override() -> bool {
    env_overrides_config(|name| std::env::var_os(name).is_some())
}

fn env_overrides_config(is_set: impl Fn(&str) -> bool) -> bool {
    is_set("GIT_CONFIG_COUNT") || is_set("GIT_CONFIG_PARAMETERS")
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
        if config_env_override() {
            return Err("git config is overridden through the environment".to_string());
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
    /// `rev-parse --abbrev-ref HEAD`
    AbbrevHead,
    /// `rev-parse [--abbrev-ref] [--symbolic-full-name] [<branch>]@{upstream}`
    Upstream {
        branch: Option<&'a str>,
        short: bool,
    },
    /// `status --porcelain`
    StatusPorcelain,
}

/// `[--abbrev-ref] [--symbolic-full-name] [<branch>]@{upstream|u}` (the part
/// of a `rev-parse` command line after `rev-parse`).
fn parse_upstream<'a>(rest: &[&'a str]) -> Option<Read<'a>> {
    let (spec, flags) = rest.split_last()?;
    if flags.is_empty() {
        return None;
    }
    let mut short = false;
    for flag in flags {
        match *flag {
            "--abbrev-ref" => short = true,
            "--symbolic-full-name" => {}
            _ => return None,
        }
    }
    let branch = match *spec {
        "@{upstream}" | "@{u}" => None,
        other => {
            let name = other
                .strip_suffix("@{upstream}")
                .or_else(|| other.strip_suffix("@{u}"))?;
            if !plain_name(name) {
                return None;
            }
            Some(name)
        }
    };
    Some(Read::Upstream { branch, short })
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
    if let ["rev-parse", rest @ ..] = args {
        if let Some(upstream) = parse_upstream(rest) {
            return Some(upstream);
        }
    }
    match args {
        ["rev-parse", "--abbrev-ref", "HEAD"] => Some(Read::AbbrevHead),
        ["status", "--porcelain"] => Some(Read::StatusPorcelain),
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
        Read::AbbrevHead => return repo.abbrev_head(),
        Read::Upstream { branch, short } => return repo.upstream(branch, short),
        Read::StatusPorcelain => return repo.status_porcelain(),
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

/// What a read printed and how it exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub stdout: String,
    pub code: i32,
}

/// A config key made only of the characters section/variable names use, with
/// at least one `.` (`remote.origin.url`).
fn plain_config_key(key: &str) -> bool {
    key.contains('.')
        && !key.starts_with('.')
        && !key.ends_with('.')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'))
}

/// Whether a system-level git config (which libgit2 may not locate the way
/// git does) could rewrite URLs: any readable well-known system config that
/// mentions `insteadOf`.
fn system_config_may_rewrite_urls() -> bool {
    let mut candidates: Vec<std::path::PathBuf> = vec![
        "C:/Program Files/Git/etc/gitconfig".into(),
        "C:/Program Files (x86)/Git/etc/gitconfig".into(),
        "C:/ProgramData/Git/config".into(),
        "/etc/gitconfig".into(),
        "/usr/local/etc/gitconfig".into(),
        "/opt/homebrew/etc/gitconfig".into(),
    ];
    if let Some(path) = std::env::var_os("GIT_CONFIG_SYSTEM") {
        candidates.push(path.into());
    }
    candidates.iter().any(|path| {
        std::fs::read_to_string(path)
            .is_ok_and(|text| text.to_ascii_lowercase().contains("insteadof"))
    })
}

/// Config files outside the repository that git would read: global, XDG and
/// the usual system locations (libgit2 locates the system one differently
/// from git, so these are read as text).
fn outside_config_files() -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let home = std::path::PathBuf::from(home);
        files.push(home.join(".gitconfig"));
        files.push(home.join(".config").join("git").join("config"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        files.push(std::path::PathBuf::from(xdg).join("git").join("config"));
    }
    for path in [
        "C:/Program Files/Git/etc/gitconfig",
        "C:/Program Files (x86)/Git/etc/gitconfig",
        "C:/ProgramData/Git/config",
        "/etc/gitconfig",
        "/usr/local/etc/gitconfig",
        "/opt/homebrew/etc/gitconfig",
    ] {
        files.push(path.into());
    }
    for var in ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
        if let Some(path) = std::env::var_os(var) {
            files.push(path.into());
        }
    }
    files
}

/// Whether a global/XDG/system config file mentions `[remote ` or pulls in
/// another file: then a remote could be defined outside the repository.
fn outside_config_may_define_remotes() -> bool {
    outside_config_files().iter().any(|file| {
        std::fs::read_to_string(file).is_ok_and(|text| {
            let text = text.to_ascii_lowercase();
            text.contains("[remote ") || text.contains("[include")
        })
    })
}

/// Every `name = value` the repository defines itself (local, then worktree
/// level), in file order. `None` for a value-less key or any read error.
fn repo_level_entries(config: &git2::Config) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    for level in [git2::ConfigLevel::Local, git2::ConfigLevel::Worktree] {
        let Ok(scoped) = config.open_level(level) else {
            continue;
        };
        let mut entries = scoped.entries(None).ok()?;
        while let Some(entry) = entries.next() {
            let entry = entry.ok()?;
            out.push((entry.name()?.to_string(), entry.value()?.to_string()));
        }
    }
    Some(out)
}

/// `git remote -v` for remotes with one URL (and at most one push URL), none
/// of which an `insteadOf` rule could rewrite: `name\turl (fetch)` then
/// `name\turl (push)`, remotes ordered by name.
fn remote_verbose(root: &Path) -> Option<Answer> {
    let repo = Repo::open(root).ok()?;
    if outside_config_may_define_remotes() || system_config_may_rewrite_urls() {
        return None;
    }
    let config = repo.repo.config().ok()?;
    let mut rewrite_prefixes = Vec::new();
    let mut rewrites = config.entries(Some("^url\\..*\\.(push)?insteadof$")).ok()?;
    while let Some(entry) = rewrites.next() {
        rewrite_prefixes.push(entry.ok()?.value()?.to_string());
    }
    let mut remotes: std::collections::BTreeMap<String, (Vec<String>, Vec<String>, bool)> =
        std::collections::BTreeMap::new();
    for (name, value) in repo_level_entries(&config)? {
        let Some(rest) = name.strip_prefix("remote.") else {
            continue;
        };
        let (remote, variable) = rest.rsplit_once('.')?;
        let slot = remotes.entry(remote.to_string()).or_default();
        match variable {
            "url" => slot.0.push(value),
            "pushurl" => slot.1.push(value),
            _ => slot.2 = true,
        }
    }
    let mut out = String::new();
    for (name, (urls, push_urls, _)) in &remotes {
        let [url] = urls.as_slice() else {
            return None;
        };
        if push_urls.len() > 1 {
            return None;
        }
        let push = push_urls.first().unwrap_or(url);
        if [url, push].iter().any(|u| {
            rewrite_prefixes
                .iter()
                .any(|prefix| !prefix.is_empty() && u.starts_with(prefix.as_str()))
        }) {
            return None;
        }
        out.push_str(&format!("{name}\t{url} (fetch)\n{name}\t{push} (push)\n"));
    }
    Some(Answer {
        stdout: out,
        code: 0,
    })
}

/// `git config --get-regexp <pattern>` for the few `remote.<name>.<variable>`
/// patterns ralphus asks, each matched by variable name rather than by a
/// regex engine. No match is git's exit 1 with no output.
fn config_regexp(root: &Path, pattern: &str) -> Option<Answer> {
    let variables: &[&str] = match pattern {
        "^remote\\..*\\.url$" => &["url"],
        "^remote\\..*\\.glab-resolved(-base|-head)?$" => {
            &["glab-resolved", "glab-resolved-base", "glab-resolved-head"]
        }
        "^remote\\..*\\.gh-resolved$" => &["gh-resolved"],
        _ => return None,
    };
    let repo = Repo::open(root).ok()?;
    if outside_config_may_define_remotes() {
        return None;
    }
    let config = repo.repo.config().ok()?;
    let mut out = String::new();
    for (name, value) in repo_level_entries(&config)? {
        let Some(rest) = name.strip_prefix("remote.") else {
            continue;
        };
        if rest
            .rsplit_once('.')
            .is_some_and(|(_, variable)| variables.contains(&variable))
        {
            out.push_str(&format!("{name} {value}\n"));
        }
    }
    let code = i32::from(out.is_empty());
    Some(Answer { stdout: out, code })
}

/// Answer `git config --get <key>` or `git remote get-url <name>` from the
/// repository's own config (worktree level, then local), without a process.
///
/// Only a value found at those levels is answered: they are the highest
/// non-environment scopes, so nothing lower can override it, and libgit2
/// reads them exactly. A missing key (which could live in a lower scope that
/// libgit2 locates differently from git), an `insteadOf` rewrite, a multiple
/// value, or any error returns `None` ("run git").
#[must_use]
pub fn try_config_read(root: &Path, args: &[&str]) -> Option<Answer> {
    match args {
        ["remote", "-v"] => return remote_verbose(root),
        ["config", "--get-regexp", pattern] => return config_regexp(root, pattern),
        _ => {}
    }
    let (key, remote) = match args {
        ["config", "--get", key] if plain_config_key(key) => ((*key).to_string(), false),
        ["remote", "get-url", name]
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) =>
        {
            (format!("remote.{name}.url"), true)
        }
        _ => return None,
    };
    let repo = Repo::open(root).ok()?;
    let config = repo.repo.config().ok()?;
    // `remote get-url` applies `url.<base>.insteadOf`: a rewrite happens only
    // when the URL starts with one of those values. Collect them, and let git
    // answer if any could apply (or if a system config, which libgit2 may not
    // locate the way git does, mentions `insteadOf` at all).
    let mut rewrite_prefixes = Vec::new();
    if remote {
        if system_config_may_rewrite_urls() {
            return None;
        }
        let mut rewrites = config.entries(Some("^url\\..*\\.insteadof$")).ok()?;
        while let Some(entry) = rewrites.next() {
            rewrite_prefixes.push(entry.ok()?.value()?.to_string());
        }
    }
    for level in [git2::ConfigLevel::Worktree, git2::ConfigLevel::Local] {
        let Ok(scoped) = config.open_level(level) else {
            continue;
        };
        let mut values = scoped.multivar(&key, None).ok()?;
        let mut found = Vec::new();
        while let Some(entry) = values.next() {
            found.push(entry.ok()?.value()?.to_string());
        }
        match found.as_slice() {
            [] => {}
            [only] => {
                if rewrite_prefixes
                    .iter()
                    .any(|prefix| !prefix.is_empty() && only.starts_with(prefix.as_str()))
                {
                    return None;
                }
                return Some(Answer {
                    stdout: format!("{only}\n"),
                    code: 0,
                });
            }
            _ => return None,
        }
    }
    None
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
        } else {
            let s = full.strip_prefix("refs/remotes/")?;
            if s.ends_with("/HEAD") {
                return None;
            }
            (4, s)
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

    /// `git rev-parse --abbrev-ref HEAD`: the checked-out branch's short name,
    /// or `HEAD` when detached.
    fn abbrev_head(&self) -> Option<String> {
        if self.repo.head_detached().ok()? {
            return Some("HEAD\n".to_string());
        }
        let head = self.repo.head().ok()?;
        let name = head.name()?.to_string();
        Some(format!("{}\n", self.shorten(&name)?))
    }

    /// `git rev-parse [--abbrev-ref] [--symbolic-full-name] [<branch>]@{upstream}`:
    /// the branch's configured upstream, short or fully qualified. A branch
    /// with no upstream (git's error) is left to git.
    fn upstream(&self, branch: Option<&str>, short: bool) -> Option<String> {
        let local = match branch {
            Some(name) => self.repo.find_branch(name, git2::BranchType::Local).ok()?,
            None => {
                let head = self.repo.head().ok()?;
                if !head.is_branch() {
                    return None;
                }
                git2::Branch::wrap(head)
            }
        };
        let full = local.upstream().ok()?.get().name()?.to_string();
        let shown = if short { self.shorten(&full)? } else { full };
        Some(format!("{shown}\n"))
    }

    /// `git status --porcelain` (v1), for the plain cases: no renames,
    /// conflicts, submodules or paths git would quote. Anything else, or a
    /// config key libgit2 does not honour (`status.showUntrackedFiles`,
    /// `status.renames`), is left to git.
    ///
    /// Every production caller only asks whether the output is empty (is the
    /// worktree dirty?), so this exists to save a process, not to produce
    /// the listing.
    ///
    /// REVERT THIS if it turns out to cause noticeable latency: delete the
    /// `Read::StatusPorcelain` arms in `parse` and `try_read` (or set
    /// `RALPHUS_GIT_INPROC=0`). Measured on Windows against the `git status`
    /// the daemon runs today (a process launch included): 248 ms vs 216 ms in
    /// this repo (huge, dirty), 141 ms vs 161 ms in a large clean checkout,
    /// 71 ms vs 33 ms in a small repo -- i.e. -40 ms to +20 ms per call. The
    /// callers are cell start with an upstream chain (`reviews::rebase_onto`),
    /// the review merge/restack loop (`guardian_merge::drive_rebase`,
    /// `worktree_has_changes`), the start and end of a feedback/auto-fix pass,
    /// and the hourly ark sweep; none runs on a board request or under the
    /// store lock.
    fn status_porcelain(&self) -> Option<String> {
        if self.repo.workdir()?.join(".gitmodules").exists() {
            return None;
        }
        let config = self.repo.config().ok()?;
        for key in [
            "status.showuntrackedfiles",
            "status.renames",
            "status.relativepaths",
        ] {
            if config.get_entry(key).is_ok() {
                return None;
            }
        }
        let mut options = StatusOptions::new();
        options
            .show(StatusShow::IndexAndWorkdir)
            .include_untracked(true)
            .recurse_untracked_dirs(false)
            .include_ignored(false)
            .include_unmodified(false)
            .renames_head_to_index(true)
            .renames_index_to_workdir(true);
        let statuses = self.repo.statuses(Some(&mut options)).ok()?;

        let mut tracked: Vec<(Vec<u8>, String)> = Vec::new();
        let mut untracked: Vec<(Vec<u8>, String)> = Vec::new();
        let (mut index_added, mut index_deleted) = (false, false);
        for entry in statuses.iter() {
            let status = entry.status();
            if status.is_ignored() || status.is_conflicted() {
                return None;
            }
            if status.intersects(Status::INDEX_RENAMED | Status::WT_RENAMED) {
                return None;
            }
            let path = entry.path_bytes().to_vec();
            // git C-quotes anything outside plain printable ASCII (and spaces).
            if !path
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"._-/+@,=%~".contains(b))
            {
                return None;
            }
            let shown = String::from_utf8(path.clone()).ok()?;
            // A file removed from the index but still on disk is one libgit2
            // entry (`INDEX_DELETED | WT_NEW`) and two lines in git's output:
            // the staged delete, and the now-untracked file.
            if status.contains(Status::WT_NEW) {
                untracked.push((path.clone(), format!("?? {shown}\n")));
                if !status.intersects(Status::INDEX_DELETED) {
                    continue;
                }
            }
            let x = if status.contains(Status::INDEX_NEW) {
                index_added = true;
                'A'
            } else if status.contains(Status::INDEX_MODIFIED) {
                'M'
            } else if status.contains(Status::INDEX_DELETED) {
                index_deleted = true;
                'D'
            } else if status.contains(Status::INDEX_TYPECHANGE) {
                'T'
            } else {
                ' '
            };
            let y = if status.contains(Status::WT_MODIFIED) {
                'M'
            } else if status.contains(Status::WT_DELETED) {
                'D'
            } else if status.contains(Status::WT_TYPECHANGE) {
                'T'
            } else {
                ' '
            };
            if x == ' ' && y == ' ' {
                return None;
            }
            tracked.push((path, format!("{x}{y} {shown}\n")));
        }
        // An added file next to a deleted one may be a rename git would
        // detect with its own similarity rules.
        if index_added && index_deleted {
            return None;
        }
        // git lists tracked changes first, then untracked, each by path.
        tracked.sort();
        untracked.sort();
        Some(
            tracked
                .into_iter()
                .chain(untracked)
                .map(|(_, line)| line)
                .collect(),
        )
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

    /// A config answer must equal git's stdout and exit code.
    fn config_same_as_git(dir: &Path, args: &[&str]) {
        let ours = try_config_read(dir, args).unwrap_or_else(|| panic!("not answered: {args:?}"));
        assert_eq!(ours.code, 0, "{args:?}");
        assert_eq!(ours.stdout, git_out(dir, args), "{args:?}");
    }

    #[test]
    fn config_get_and_remote_get_url_match_git() {
        let dir = temp_repo("config");
        run(
            &dir,
            &["remote", "add", "origin", "git@gitlab.com:acme/w.git"],
        );
        run(
            &dir,
            &["remote", "add", "fork-a_b.c", "https://example.com/x/y.git"],
        );
        run(&dir, &["config", "branch.main.remote", "origin"]);
        run(
            &dir,
            &[
                "config",
                "ralphus.note",
                "has spaces and \"quotes\" and \\ slash",
            ],
        );
        config_same_as_git(&dir, &["remote", "get-url", "origin"]);
        config_same_as_git(&dir, &["remote", "get-url", "fork-a_b.c"]);
        config_same_as_git(&dir, &["config", "--get", "remote.origin.url"]);
        config_same_as_git(&dir, &["config", "--get", "branch.main.remote"]);
        config_same_as_git(&dir, &["config", "--get", "ralphus.note"]);
        // Section and variable names are case-insensitive in git.
        config_same_as_git(&dir, &["config", "--get", "Remote.origin.URL"]);
        // Missing keys and unknown remotes are git's to report.
        assert!(try_config_read(&dir, &["config", "--get", "remote.nope.url"]).is_none());
        assert!(try_config_read(&dir, &["remote", "get-url", "nope"]).is_none());
        // Not a shape this answers.
        assert!(try_config_read(&dir, &["config", "--get-all", "remote.origin.url"]).is_none());
        assert!(try_config_read(&dir, &["config", "--get", "nodots"]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remote_verbose_matches_git() {
        let dir = temp_repo("remote-v");
        // No remotes: empty output.
        config_same_as_git(&dir, &["remote", "-v"]);
        run(
            &dir,
            &["remote", "add", "zeta", "https://example.com/z.git"],
        );
        run(
            &dir,
            &["remote", "add", "origin", "git@gitlab.com:acme/w.git"],
        );
        run(
            &dir,
            &["remote", "add", "fork-a_b.c", "https://example.com/f.git"],
        );
        run(
            &dir,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                "git@gitlab.com:acme/push.git",
            ],
        );
        config_same_as_git(&dir, &["remote", "-v"]);
        // Two fetch URLs: ordering rules are git's.
        run(
            &dir,
            &[
                "remote",
                "set-url",
                "--add",
                "zeta",
                "https://example.com/z2.git",
            ],
        );
        assert!(try_config_read(&dir, &["remote", "-v"]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn config_get_regexp_matches_git_including_no_match() {
        let dir = temp_repo("regexp");
        let forms = [
            "^remote\\..*\\.url$",
            "^remote\\..*\\.glab-resolved(-base|-head)?$",
            "^remote\\..*\\.gh-resolved$",
        ];
        // Nothing configured: git exits 1 with no output.
        for pattern in forms {
            let ours = try_config_read(&dir, &["config", "--get-regexp", pattern]).unwrap();
            assert_eq!((ours.stdout.as_str(), ours.code), ("", 1), "{pattern}");
        }
        run(
            &dir,
            &["remote", "add", "origin", "git@gitlab.com:acme/w.git"],
        );
        run(
            &dir,
            &["remote", "add", "fork.dotted", "https://example.com/f.git"],
        );
        run(
            &dir,
            &["config", "remote.origin.glab-resolved", "gitlab.com/acme/w"],
        );
        run(
            &dir,
            &["config", "remote.origin.glab-resolved-base", "main"],
        );
        run(
            &dir,
            &["config", "remote.origin.glab-resolved-head", "side"],
        );
        run(&dir, &["config", "remote.fork.dotted.gh-resolved", "x/y"]);
        for pattern in forms {
            config_same_as_git(&dir, &["config", "--get-regexp", pattern]);
        }
        // Patterns outside the list are not this answer.
        assert!(try_config_read(&dir, &["config", "--get-regexp", "^core\\."]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn multiple_values_and_url_rewrites_are_left_to_git() {
        let dir = temp_repo("config-odd");
        run(
            &dir,
            &["remote", "add", "origin", "git@gitlab.com:acme/w.git"],
        );
        run(
            &dir,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/heads/x:refs/remotes/origin/x",
            ],
        );
        assert!(try_config_read(&dir, &["config", "--get", "remote.origin.fetch"]).is_none());
        config_same_as_git(&dir, &["remote", "get-url", "origin"]);
        run(
            &dir,
            &[
                "config",
                "url.https://gitlab.com/.insteadOf",
                "git@gitlab.com:",
            ],
        );
        // get-url would print the rewritten URL: git decides.
        assert!(try_config_read(&dir, &["remote", "get-url", "origin"]).is_none());
        // A plain config read is not rewritten, so it is still answered.
        config_same_as_git(&dir, &["config", "--get", "remote.origin.url"]);
        // A remote no rewrite rule matches is answered even though rules exist.
        run(
            &dir,
            &["remote", "add", "other", "https://example.com/z.git"],
        );
        config_same_as_git(&dir, &["remote", "get-url", "other"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_worktree_level_value_overrides_the_local_one_like_git() {
        let dir = temp_repo("config-wt");
        run(&dir, &["config", "extensions.worktreeConfig", "true"]);
        run(&dir, &["config", "ralphus.who", "local"]);
        let linked = dir.with_file_name(format!(
            "{}-linked",
            dir.file_name().unwrap().to_string_lossy()
        ));
        run(
            &dir,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );
        run(
            &linked,
            &["config", "--worktree", "ralphus.who", "worktree"],
        );
        config_same_as_git(&linked, &["config", "--get", "ralphus.who"]);
        config_same_as_git(&dir, &["config", "--get", "ralphus.who"]);
        assert_eq!(
            try_config_read(&linked, &["config", "--get", "ralphus.who"])
                .unwrap()
                .stdout,
            "worktree\n"
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(linked);
    }

    #[test]
    fn environment_injected_config_disables_the_config_dependent_answers() {
        assert!(!env_overrides_config(|_| false));
        assert!(env_overrides_config(|name| name == "GIT_CONFIG_COUNT"));
        assert!(env_overrides_config(|name| name == "GIT_CONFIG_PARAMETERS"));
        assert!(!env_overrides_config(|name| name == "GIT_CONFIG_NOSYSTEM"));
    }

    #[test]
    fn upstream_and_abbrev_head_match_git() {
        let dir = temp_repo("upstream");
        // A remote-tracking upstream (the remote needs its fetch refspec to
        // map the branch, as in git) and a local-branch upstream.
        run(&dir, &["remote", "add", "origin", "file:///nowhere"]);
        run(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run(&dir, &["config", "branch.main.remote", "origin"]);
        run(&dir, &["config", "branch.main.merge", "refs/heads/main"]);
        run(&dir, &["branch", "side"]);
        run(&dir, &["branch", "--set-upstream-to=main", "side"]);
        for args in [
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ][..],
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
            &["rev-parse", "--symbolic-full-name", "@{upstream}"],
            &["rev-parse", "--abbrev-ref", "@{upstream}"],
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "main@{u}",
            ],
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "side@{upstream}",
            ],
            &["rev-parse", "--symbolic-full-name", "side@{u}"],
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ] {
            same_as_git(&dir, args);
        }
        // No upstream configured: git's error to report.
        assert!(
            try_read(
                &dir,
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "nope@{u}"
                ]
            )
            .is_none()
        );
        run(&dir, &["checkout", "-q", "side"]);
        same_as_git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        run(&dir, &["checkout", "-q", "--detach"]);
        same_as_git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert!(
            try_read(
                &dir,
                &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]
            )
            .is_none()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn status_porcelain_matches_git_across_states() {
        let dir = temp_repo("status");
        let status = ["status", "--porcelain"];
        // Clean.
        same_as_git(&dir, &status);
        assert_eq!(try_read(&dir, &status).unwrap(), "");
        // Worktree edit, staged edit, both, staged new, deleted, untracked file
        // and directory, nested repo, ignored file.
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        std::fs::write(dir.join("c.txt"), "c\n").unwrap();
        std::fs::write(dir.join("d.txt"), "d\n").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "more"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap(); // worktree edit
        std::fs::write(dir.join("b.txt"), "b2\n").unwrap();
        run(&dir, &["add", "b.txt"]); // staged edit
        std::fs::write(dir.join("c.txt"), "c2\n").unwrap();
        run(&dir, &["add", "c.txt"]);
        std::fs::write(dir.join("c.txt"), "c3\n").unwrap(); // staged + worktree edit
        std::fs::remove_file(dir.join("d.txt")).unwrap(); // worktree delete
        std::fs::write(dir.join("new.txt"), "n\n").unwrap();
        run(&dir, &["add", "new.txt"]); // staged new
        std::fs::write(dir.join("zeta.txt"), "z\n").unwrap(); // untracked
        std::fs::create_dir_all(dir.join("adir/sub")).unwrap();
        std::fs::write(dir.join("adir/sub/x.txt"), "x\n").unwrap(); // untracked dir
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.join("skip.log"), "ignored\n").unwrap();
        same_as_git(&dir, &status);
        // A staged add next to a staged delete might be a rename git would
        // detect itself: left to git.
        run(&dir, &["rm", "-q", "--cached", "a.txt"]);
        assert!(try_read(&dir, &status).is_none(), "possible rename");
        let _ = std::fs::remove_dir_all(dir);

        // A staged delete on its own (the file stays on disk, so git also
        // lists it as untracked) is two lines for one libgit2 entry.
        let dir = temp_repo("status-staged-delete");
        run(&dir, &["rm", "-q", "--cached", "a.txt"]);
        same_as_git(&dir, &status);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn status_porcelain_leaves_renames_odd_paths_and_special_config_to_git() {
        let dir = temp_repo("status-odd");
        let status = ["status", "--porcelain"];
        // A rename: git reports `R`, libgit2's heuristics may differ.
        run(&dir, &["mv", "a.txt", "renamed.txt"]);
        assert!(try_read(&dir, &status).is_none(), "rename");
        run(&dir, &["reset", "-q", "--hard"]);
        same_as_git(&dir, &status);
        // A path git would quote.
        std::fs::write(dir.join("has space.txt"), "x").unwrap();
        assert!(try_read(&dir, &status).is_none(), "quoted path");
        std::fs::remove_file(dir.join("has space.txt")).unwrap();
        // Config libgit2 does not honour.
        run(&dir, &["config", "status.showUntrackedFiles", "no"]);
        assert!(try_read(&dir, &status).is_none(), "status config");
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
