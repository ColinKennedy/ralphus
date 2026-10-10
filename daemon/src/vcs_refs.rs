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
//!
//! [`cached_config_read`] / [`remember_config_read`] memoize the other
//! read-only question the same callers repeat: `remote get-url <name>` and
//! `config --get <key>`. Their answers depend only on git config files, so a
//! remembered answer is reused only while the repository's `config` (and
//! `config.worktree`), the global `~/.gitconfig` and the XDG git config all
//! keep the modification times they had when it was recorded, and for at
//! most [`CONFIG_CACHE_TTL`] (which also bounds `[include]`d files).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::time::{Duration, Instant, SystemTime};

/// Longest a remembered config read is reused, whatever the file times say.
const CONFIG_CACHE_TTL: Duration = Duration::from_secs(30);

/// Whether `args` is a config read [`cached_config_read`] memoizes.
fn is_cacheable_config_read(args: &[&str]) -> bool {
    matches!(args, ["remote", "get-url", name] if !name.starts_with('-'))
        || matches!(args, ["config", "--get", key] if !key.starts_with('-'))
}

/// Modification times of every config file a read in `root` can depend on.
fn config_stamp(root: &Path) -> Option<Vec<Option<SystemTime>>> {
    let (git_dir, common) = git_dirs(root)?;
    let mtime = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    Some(vec![
        mtime(common.join("config")),
        mtime(git_dir.join("config.worktree")),
        home.and_then(|h| mtime(h.join(".gitconfig"))),
        xdg.and_then(|x| mtime(x.join("git").join("config"))),
    ])
}

type ConfigCacheKey = (PathBuf, Vec<String>);
type ConfigCacheEntry = (Instant, Vec<Option<SystemTime>>, Output);

fn config_cache() -> &'static parking_lot::Mutex<HashMap<ConfigCacheKey, ConfigCacheEntry>> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<HashMap<ConfigCacheKey, ConfigCacheEntry>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn config_key(root: &Path, args: &[&str]) -> ConfigCacheKey {
    (
        root.to_path_buf(),
        args.iter().map(|a| (*a).to_string()).collect(),
    )
}

/// The two full commit SHAs of a `merge-base --is-ancestor <a> <b>` call.
/// Whether one commit is an ancestor of another never changes, so a definite
/// answer for a pair of full SHAs is reusable forever.
fn ancestry_pair<'a>(args: &[&'a str]) -> Option<(&'a str, &'a str)> {
    match args {
        ["merge-base", "--is-ancestor", a, b] if is_full_sha(a) && is_full_sha(b) => Some((a, b)),
        _ => None,
    }
}

/// (repository common dir, ancestor SHA, descendant SHA).
type AncestryKey = (PathBuf, String, String);

/// Definite `--is-ancestor` answers (exit 0 or 1) per repository common dir.
fn ancestry_cache() -> &'static parking_lot::Mutex<HashMap<AncestryKey, bool>> {
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<HashMap<AncestryKey, bool>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Upper bound on remembered ancestry answers; the map is cleared when full.
const ANCESTRY_CACHE_MAX: usize = 50_000;

/// A remembered `merge-base --is-ancestor <sha> <sha>` answer for `root`'s
/// repository.
#[must_use]
pub fn cached_ancestry(root: &Path, args: &[&str]) -> Option<Output> {
    let (a, b) = ancestry_pair(args)?;
    let (_, common) = git_dirs(root)?;
    let is_ancestor = *ancestry_cache()
        .lock()
        .get(&(common, a.to_string(), b.to_string()))?;
    Some(Output {
        status: exit_status(u32::from(!is_ancestor)),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

/// Remember git's definite answer (exit 0 or 1) to an `--is-ancestor` call
/// on two full SHAs; errors (e.g. an unknown object) are never remembered.
pub fn remember_ancestry(root: &Path, args: &[&str], output: &Output) {
    let Some((a, b)) = ancestry_pair(args) else {
        return;
    };
    let is_ancestor = match output.status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => return,
    };
    let Some((_, common)) = git_dirs(root) else {
        return;
    };
    let mut cache = ancestry_cache().lock();
    if cache.len() >= ANCESTRY_CACHE_MAX {
        cache.clear();
    }
    cache.insert((common, a.to_string(), b.to_string()), is_ancestor);
}

/// A remembered answer to a config read (see the module doc), if every file
/// it depends on is unchanged and it is younger than [`CONFIG_CACHE_TTL`].
#[must_use]
pub fn cached_config_read(root: &Path, args: &[&str]) -> Option<Output> {
    if !is_cacheable_config_read(args) {
        return None;
    }
    let stamp = config_stamp(root)?;
    let cache = config_cache().lock();
    let (at, recorded, output) = cache.get(&config_key(root, args))?;
    (at.elapsed() < CONFIG_CACHE_TTL && *recorded == stamp).then(|| output.clone())
}

/// The config-file times to record a config read under, taken *before* git
/// runs so a file changed during the read can never be paired with the
/// answer from before the change. `None` for anything not memoized.
#[must_use]
pub fn config_read_stamp(root: &Path, args: &[&str]) -> Option<Vec<Option<SystemTime>>> {
    if !is_cacheable_config_read(args) {
        return None;
    }
    config_stamp(root)
}

/// Remember git's answer to a config read for [`cached_config_read`], under
/// the [`config_read_stamp`] taken before it ran.
pub fn remember_config_read(
    root: &Path,
    args: &[&str],
    stamp: Vec<Option<SystemTime>>,
    output: &Output,
) {
    let mut cache = config_cache().lock();
    cache.retain(|_, (at, _, _)| at.elapsed() < CONFIG_CACHE_TTL);
    cache.insert(
        config_key(root, args),
        (Instant::now(), stamp, output.clone()),
    );
}

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

/// The per-worktree git dir of the checkout whose top is `root`: `root/.git`
/// when that is a directory, or the target of a linked worktree's
/// `gitdir:` file. Files kept there belong to the checkout but are never
/// part of its working tree, so nothing can stage or commit them.
#[must_use]
pub fn worktree_git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let path = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

/// The repository's per-worktree git dir and its common dir, for a `root`
/// that is the top of a checkout (`root/.git` is a directory or a `gitdir:`
/// file). `None` for anything else, or a reftable repository.
fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let git_dir = worktree_git_dir(root)?;
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
        // `NotADirectory`: a parent component is a file, e.g. the
        // `refs/remotes/origin/main/HEAD` candidate when
        // `refs/remotes/origin/main` is a loose ref (Unix reports ENOTDIR).
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
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

/// `/`-separated form of a path, the way git prints worktree paths.
fn slash(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    text.strip_prefix("//?/")
        .map_or(text.clone(), str::to_string)
}

/// One `git worktree list --porcelain` record, or `None` when the worktree is
/// anything but plainly healthy: HEAD unreadable or unresolvable, locked, or
/// (for a linked worktree) a `gitdir` file that is missing, relative, or
/// points nowhere -- git annotates those (`locked`, `prunable`, ...) and this
/// module does not reproduce the annotations.
fn worktree_record(path: &str, git_dir: &Path, common: &Path) -> Option<String> {
    // `locked [<reason>]`, exactly as git prints it: the reason file trimmed.
    // git C-quotes a reason holding control characters, quotes, backslashes or
    // non-ASCII text; those are left to git.
    let locked = match std::fs::read_to_string(git_dir.join("locked")) {
        Ok(text) => {
            let reason = text.trim();
            if reason
                .chars()
                .any(|c| c.is_control() || c == '"' || c == '\\' || !c.is_ascii())
            {
                return None;
            }
            Some(if reason.is_empty() {
                "locked\n".to_string()
            } else {
                format!("locked {reason}\n")
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    let head_text = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head_text = head_text.trim();
    let Lookup::Sha(sha) = lookup(git_dir, common, "HEAD", 0) else {
        return None;
    };
    let state = match head_text.strip_prefix("ref:") {
        Some(target) => format!("branch {}", target.trim()),
        None if is_full_sha(head_text) => "detached".to_string(),
        None => return None,
    };
    Some(format!(
        "worktree {path}\nHEAD {sha}\n{state}\n{}\n",
        locked.unwrap_or_default()
    ))
}

/// Answer `git worktree list --porcelain` by reading the repository's
/// worktree admin directories, for a non-bare repository whose every
/// worktree is plainly healthy (see [`worktree_record`]). `None` means "run
/// git". Linked worktrees are listed in directory order, the order git
/// itself reads them in.
#[must_use]
pub fn try_worktree_list(root: &Path, args: &[&str]) -> Option<Output> {
    if args != ["worktree", "list", "--porcelain"] {
        return None;
    }
    let (_, common) = git_dirs(root)?;
    // A linked worktree names the shared dir relatively (`../..`).
    let common = std::fs::canonicalize(common).ok()?;
    if common.file_name()? != ".git" {
        return None;
    }
    let config = std::fs::read_to_string(common.join("config")).ok()?;
    if config
        .lines()
        .any(|l| l.trim().replace(' ', "").eq_ignore_ascii_case("bare=true"))
    {
        return None;
    }
    let main_path = slash(&std::fs::canonicalize(common.parent()?).ok()?);
    let mut out = worktree_record(&main_path, &common, &common)?;
    let admin = common.join("worktrees");
    let entries = match std::fs::read_dir(&admin) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(success_output(out));
        }
        Err(_) => return None,
    };
    let mut linked: Vec<(String, String)> = Vec::new();
    for entry in entries {
        let dir = entry.ok()?.path();
        if !dir.is_dir() {
            return None;
        }
        let gitdir = std::fs::read_to_string(dir.join("gitdir")).ok()?;
        let gitdir = gitdir.trim();
        let checkout = gitdir
            .strip_suffix("/.git")
            .or_else(|| gitdir.strip_suffix("\\.git"))?;
        if !Path::new(gitdir).is_absolute() || !Path::new(gitdir).exists() {
            return None;
        }
        let path = slash(Path::new(checkout));
        let record = worktree_record(&path, &dir, &common)?;
        linked.push((path, record));
    }
    // git lists the main worktree first, then the linked ones ordered by
    // path (`fspathcmp`: case-insensitive when `core.ignorecase` is set).
    let ignore_case = config.lines().any(|l| {
        l.trim()
            .replace(' ', "")
            .eq_ignore_ascii_case("ignorecase=true")
    });
    if ignore_case {
        linked.sort_by_cached_key(|(path, _)| path.to_ascii_lowercase());
    } else {
        linked.sort_by(|a, b| a.0.cmp(&b.0));
    }
    for (_, record) in linked {
        out.push_str(&record);
    }
    Some(success_output(out))
}

/// `/`-separated absolute form of `path`, the way git prints it.
fn absolute_display(path: &Path) -> Option<String> {
    std::fs::canonicalize(path).ok().map(|p| slash(&p))
}

/// Whether any config file git would read for `root` could set
/// `core.hooksPath` (or pull in another file that might): the repo's own
/// `config`/`config.worktree`, the global and XDG files, and the usual system
/// locations. Conservative -- any mention of `hookspath` or an `[include`
/// counts.
fn hooks_path_may_be_configured(git_dir: &Path, common: &Path) -> bool {
    // Config injected through the environment (`GIT_CONFIG_COUNT`, `git -c`)
    // outranks every file and is invisible here.
    if ralphus_runner::git_inproc::config_env_override() {
        return true;
    }
    let mut files = vec![common.join("config"), git_dir.join("config.worktree")];
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let home = PathBuf::from(home);
        files.push(home.join(".gitconfig"));
        files.push(home.join(".config").join("git").join("config"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        files.push(PathBuf::from(xdg).join("git").join("config"));
    }
    for path in [
        "C:/Program Files/Git/etc/gitconfig",
        "C:/Program Files (x86)/Git/etc/gitconfig",
        "C:/ProgramData/Git/config",
        "/etc/gitconfig",
        "/usr/local/etc/gitconfig",
        "/opt/homebrew/etc/gitconfig",
    ] {
        files.push(PathBuf::from(path));
    }
    for var in ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
        if let Some(path) = std::env::var_os(var) {
            files.push(PathBuf::from(path));
        }
    }
    files.iter().any(|file| {
        std::fs::read_to_string(file).is_ok_and(|text| {
            let text = text.to_ascii_lowercase();
            text.contains("hookspath") || text.contains("[include")
        })
    })
}

/// Answer the `rev-parse` questions that are a function of the checkout's
/// on-disk layout -- `--git-common-dir`, `--git-dir`, `--show-toplevel`,
/// `--is-inside-work-tree`, `--git-path hooks` -- for a `root` that is the top
/// of a checkout, formatted the way git prints them from that directory.
/// `--git-path hooks` is left to git whenever any config file might set
/// `core.hooksPath`. `None` means "run git".
#[must_use]
pub fn try_rev_parse_layout(root: &Path, args: &[&str]) -> Option<Output> {
    let (git_dir, common) = git_dirs(root)?;
    let main = root.join(".git").is_dir();
    let stdout = match args {
        ["rev-parse", "--path-format=absolute", "--git-common-dir"] => {
            format!("{}\n", absolute_display(&common)?)
        }
        ["rev-parse", "--git-dir"] => {
            if main {
                ".git\n".to_string()
            } else {
                format!("{}\n", absolute_display(&git_dir)?)
            }
        }
        ["rev-parse", "--show-toplevel"] => format!("{}\n", absolute_display(root)?),
        ["rev-parse", "--is-inside-work-tree"] => "true\n".to_string(),
        ["rev-parse", "--git-path", "hooks"] => {
            if hooks_path_may_be_configured(&git_dir, &common) {
                return None;
            }
            if main {
                ".git/hooks\n".to_string()
            } else {
                format!("{}/hooks\n", absolute_display(&common)?)
            }
        }
        _ => return None,
    };
    Some(success_output(stdout))
}

/// A process result with `stdout` and exit `code`, for answers produced
/// without running git.
#[must_use]
pub fn answer_output(stdout: String, code: i32) -> Output {
    Output {
        status: exit_status(u32::try_from(code).unwrap_or(1)),
        stdout: stdout.into_bytes(),
        stderr: Vec::new(),
    }
}

/// A successful process result carrying `stdout`, for answers produced
/// without running git.
#[must_use]
pub fn success_output(stdout: String) -> Output {
    Output {
        status: exit_status(0),
        stdout: stdout.into_bytes(),
        stderr: Vec::new(),
    }
}

/// Per-worktree state files and directories `rev-parse --git-path` can place
/// without consulting config. `hooks` is excluded on purpose: `core.hooksPath`
/// redirects it.
const GIT_PATH_NAMES: &[&str] = &[
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REBASE_HEAD",
    "REVERT_HEAD",
];

/// Answer `git rev-parse --git-path <name>` for one of [`GIT_PATH_NAMES`]
/// from the checkout's layout, the way git prints it: `.git/<name>` for a
/// checkout whose `.git` is a directory, and the absolute path under the
/// linked worktree's admin directory otherwise. `None` means "run git".
#[must_use]
pub fn try_git_path(root: &Path, args: &[&str]) -> Option<Output> {
    let ["rev-parse", "--git-path", name] = args else {
        return None;
    };
    if !GIT_PATH_NAMES.contains(name) {
        return None;
    }
    let (git_dir, _common) = git_dirs(root)?;
    let stdout = if root.join(".git").is_dir() {
        format!(".git/{name}\n")
    } else {
        format!("{}/{name}\n", git_dir.to_string_lossy().replace('\\', "/"))
    };
    Some(success_output(stdout))
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
    fn a_remembered_config_read_is_dropped_when_the_config_file_changes() {
        let root = repo("config");
        assert!(
            git(
                &root,
                &["remote", "add", "origin", "https://example.com/a.git"]
            )
            .status
            .success()
        );
        let args = ["remote", "get-url", "origin"];
        assert!(cached_config_read(&root, &args).is_none());
        let stamp = config_read_stamp(&root, &args).expect("cacheable");
        let first = git(&root, &args);
        remember_config_read(&root, &args, stamp, &first);
        assert_eq!(
            cached_config_read(&root, &args).map(|o| o.stdout),
            Some(first.stdout.clone())
        );

        assert!(
            git(
                &root,
                &["remote", "set-url", "origin", "https://example.com/b.git"]
            )
            .status
            .success()
        );
        assert!(
            cached_config_read(&root, &args).is_none(),
            "a config edit must invalidate the remembered answer"
        );
        assert!(config_read_stamp(&root, &["remote", "-v"]).is_none());
        assert!(config_read_stamp(&root, &["config", "--get-regexp", "x"]).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn definite_ancestry_answers_between_full_shas_are_remembered() {
        let root = repo("ancestry");
        let first = String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout).unwrap();
        std::fs::write(root.join("b.txt"), "b").expect("write");
        assert!(git(&root, &["add", "b.txt"]).status.success());
        assert!(git(&root, &["commit", "-q", "-m", "two"]).status.success());
        let second = String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout).unwrap();
        let (first, second) = (first.trim(), second.trim());

        for (a, b) in [(first, second), (second, first)] {
            let args = ["merge-base", "--is-ancestor", a, b];
            assert!(cached_ancestry(&root, &args).is_none());
            let real = git(&root, &args);
            remember_ancestry(&root, &args, &real);
            let cached = cached_ancestry(&root, &args).expect("remembered");
            assert_eq!(cached.status.success(), real.status.success(), "{a}..{b}");
        }
        // Names, not SHAs, are never remembered: what they point at moves.
        let named = ["merge-base", "--is-ancestor", "HEAD", "main"];
        remember_ancestry(&root, &named, &git(&root, &named));
        assert!(cached_ancestry(&root, &named).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    fn same_path(a: &str, b: &str) -> bool {
        let canon = |s: &str| {
            std::fs::canonicalize(s)
                .or_else(|_| {
                    let p = Path::new(s);
                    std::fs::canonicalize(p.parent().unwrap_or(p))
                        .map(|d| d.join(p.file_name().unwrap_or_default()))
                })
                .unwrap_or_else(|_| PathBuf::from(s))
        };
        canon(a) == canon(b)
    }

    #[test]
    fn git_path_matches_git_for_a_main_checkout_and_a_linked_worktree() {
        let root = repo("gitpath");
        for name in GIT_PATH_NAMES {
            let args = ["rev-parse", "--git-path", name];
            let ours = try_git_path(&root, &args).expect("answered");
            let real = git(&root, &args);
            assert_eq!(
                String::from_utf8_lossy(&ours.stdout).trim(),
                String::from_utf8_lossy(&real.stdout).trim(),
                "main checkout {name}"
            );
        }

        let linked = root.with_file_name(format!(
            "{}-linked",
            root.file_name().unwrap().to_string_lossy()
        ));
        assert!(
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "side",
                    linked.to_str().unwrap()
                ]
            )
            .status
            .success()
        );
        for name in GIT_PATH_NAMES {
            let args = ["rev-parse", "--git-path", name];
            let ours = try_git_path(&linked, &args).expect("answered");
            let real = git(&linked, &args);
            let (ours, real) = (
                String::from_utf8_lossy(&ours.stdout).trim().to_string(),
                String::from_utf8_lossy(&real.stdout).trim().to_string(),
            );
            assert!(Path::new(&ours).is_absolute(), "{ours}");
            assert!(same_path(&ours, &real), "linked {name}: {ours} vs {real}");
        }

        // Names git resolves through config, or that are not per-worktree
        // state files, are left to git.
        assert!(try_git_path(&root, &["rev-parse", "--git-path", "hooks"]).is_none());
        assert!(try_git_path(&root, &["rev-parse", "--git-path", "objects"]).is_none());
        // A directory that is not the top of a checkout is left to git.
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(try_git_path(&sub, &["rev-parse", "--git-path", "rebase-merge"]).is_none());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(linked);
    }

    fn add_worktree(root: &Path, name: &str, extra: &[&str]) -> PathBuf {
        let dir = root.with_file_name(format!(
            "{}-{name}",
            root.file_name().unwrap().to_string_lossy()
        ));
        let mut args = vec!["worktree", "add", "-q"];
        args.extend_from_slice(extra);
        args.push(dir.to_str().unwrap());
        let out = git(root, &args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        dir
    }

    #[test]
    fn layout_questions_match_git_for_a_main_checkout_and_a_linked_worktree() {
        let root = repo("layout");
        let linked = add_worktree(&root, "l", &["-b", "layout-side"]);
        let forms: [&[&str]; 5] = [
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            &["rev-parse", "--git-dir"],
            &["rev-parse", "--show-toplevel"],
            &["rev-parse", "--is-inside-work-tree"],
            &["rev-parse", "--git-path", "hooks"],
        ];
        for from in [&root, &linked] {
            for args in forms {
                let ours = try_rev_parse_layout(from, args).expect("answered");
                let real = git(from, args);
                assert_eq!(
                    String::from_utf8_lossy(&ours.stdout),
                    String::from_utf8_lossy(&real.stdout),
                    "{args:?} from {}",
                    from.display()
                );
            }
        }
        // A directory that is not the top of a checkout is git's to answer.
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(try_rev_parse_layout(&sub, forms[0]).is_none());
        // Other shapes are not this answer.
        assert!(try_rev_parse_layout(&root, &["rev-parse", "HEAD"]).is_none());
        assert!(try_rev_parse_layout(&root, &["rev-parse", "--git-path", "objects"]).is_none());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(linked);
    }

    #[test]
    fn a_configured_hooks_path_sends_the_hooks_question_to_git() {
        let root = repo("hooks-path");
        let args = ["rev-parse", "--git-path", "hooks"];
        assert!(try_rev_parse_layout(&root, &args).is_some());
        assert!(
            git(&root, &["config", "core.hooksPath", "custom-hooks"])
                .status
                .success()
        );
        assert!(try_rev_parse_layout(&root, &args).is_none());
        // The questions that do not depend on config are still answered.
        assert!(try_rev_parse_layout(&root, &["rev-parse", "--git-dir"]).is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn worktree_list_matches_git_byte_for_byte() {
        let root = repo("wtlist");
        let args = ["worktree", "list", "--porcelain"];
        // Only the main worktree.
        let ours = try_worktree_list(&root, &args).expect("answered");
        assert_eq!(ours.stdout, git(&root, &args).stdout, "main only");

        let a = add_worktree(&root, "a", &["-b", "feature/a"]);
        let b = add_worktree(&root, "b", &["--detach"]);
        let c = add_worktree(&root, "c", &["-b", "c"]);
        for from in [&root, &a, &b, &c] {
            let ours = try_worktree_list(from, &args).expect("answered");
            let real = git(from, &args);
            assert_eq!(
                String::from_utf8_lossy(&ours.stdout),
                String::from_utf8_lossy(&real.stdout),
                "from {}",
                from.display()
            );
        }
        for dir in [a, b, c] {
            let _ = std::fs::remove_dir_all(dir);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn worktree_list_defers_to_git_for_locked_and_orphaned_worktrees() {
        let root = repo("wtlist-odd");
        let args = ["worktree", "list", "--porcelain"];
        let a = add_worktree(&root, "a", &["-b", "a"]);
        let b = add_worktree(&root, "b", &["-b", "b"]);
        assert!(try_worktree_list(&root, &args).is_some());

        // Locked with and without a reason: answered, byte for byte.
        let path = a.to_str().unwrap();
        for lock in [
            vec!["worktree", "lock", path],
            vec!["worktree", "unlock", path],
            vec![
                "worktree",
                "lock",
                "--reason",
                "claude session x (pid 12)",
                path,
            ],
        ] {
            assert!(git(&root, &lock).status.success(), "{lock:?}");
            let ours = try_worktree_list(&root, &args).expect("locked is answered");
            assert_eq!(
                String::from_utf8_lossy(&ours.stdout),
                String::from_utf8_lossy(&git(&root, &args).stdout),
                "{lock:?}"
            );
        }
        // A reason git would C-quote is left to git.
        assert!(git(&root, &["worktree", "unlock", path]).status.success());
        assert!(
            git(&root, &["worktree", "lock", "--reason", "say \"hi\"", path])
                .status
                .success()
        );
        assert!(try_worktree_list(&root, &args).is_none(), "quoted reason");
        assert!(git(&root, &["worktree", "unlock", path]).status.success());
        assert!(try_worktree_list(&root, &args).is_some());

        // The checkout vanished but its admin directory is still registered:
        // git marks it prunable, which this module does not reproduce.
        std::fs::remove_dir_all(&b).unwrap();
        assert!(try_worktree_list(&root, &args).is_none(), "orphaned");

        // Other shapes are not this answer.
        assert!(try_worktree_list(&root, &["worktree", "list"]).is_none());
        assert!(try_worktree_list(&root.join("sub"), &args).is_none());
        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Opt-in: `RALPHUS_PARITY_REPO=<path> cargo test ... -- --ignored
    /// worktree_list_matches_git_on_a_real_repository` compares against a
    /// repository with many real worktrees.
    #[test]
    #[ignore = "needs RALPHUS_PARITY_REPO pointing at a real repository"]
    fn worktree_list_matches_git_on_a_real_repository() {
        let root = PathBuf::from(std::env::var("RALPHUS_PARITY_REPO").expect("repo path"));
        let args = ["worktree", "list", "--porcelain"];
        let started = Instant::now();
        let ours = try_worktree_list(&root, &args);
        let ours_ms = started.elapsed().as_millis();
        let started = Instant::now();
        let real = git(&root, &args);
        let real_ms = started.elapsed().as_millis();
        eprintln!("in-process {ours_ms} ms, git {real_ms} ms");
        // The layout questions, from the repository and from a real linked worktree.
        let mut tops = vec![root.clone()];
        if let Ok(extra) = std::env::var("RALPHUS_PARITY_LINKED") {
            tops.push(PathBuf::from(extra));
        }
        for top in tops {
            for form in [
                &["rev-parse", "--path-format=absolute", "--git-common-dir"][..],
                &["rev-parse", "--git-dir"],
                &["rev-parse", "--show-toplevel"],
                &["rev-parse", "--is-inside-work-tree"],
                &["rev-parse", "--git-path", "hooks"],
            ] {
                let ours = try_rev_parse_layout(&top, form).expect("answered");
                assert_eq!(
                    String::from_utf8_lossy(&ours.stdout),
                    String::from_utf8_lossy(&git(&top, form).stdout),
                    "{form:?} from {}",
                    top.display()
                );
            }
        }
        match ours {
            None => eprintln!("fell back to git (an unusual worktree is registered)"),
            Some(ours) => assert_eq!(
                String::from_utf8_lossy(&ours.stdout),
                String::from_utf8_lossy(&real.stdout)
            ),
        }
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
