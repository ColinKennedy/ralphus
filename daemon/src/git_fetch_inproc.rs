//! `git fetch` answered in-process with libgit2 instead of a `git` subprocess.
//!
//! Every fetch the daemon issues has one shape --
//! `fetch --no-write-fetch-head <remote> +<src>:<dst>...` -- and the review,
//! PR and base-branch pollers issue it constantly. On Windows each subprocess
//! fetch is four or five processes (the `git` launcher, the real git, a console
//! host, the ssh or https transport, and `rev-list` for the connectivity check),
//! so the pollers alone keep a steady stream of short-lived processes going
//! while nothing else is happening.
//!
//! [`try_fetch`] reproduces that one shape exactly and returns `None` -- run
//! real git -- for anything it cannot:
//!
//! - **Transport.** A local path or `file://` remote is fetched directly. A
//!   GitHub or GitLab remote is fetched over HTTPS with the forge token the
//!   daemon already uses for that same repository (see [`remember_token`]),
//!   even when the configured URL is ssh: libgit2 does not read
//!   `~/.ssh/config`, the ssh agent, or git's credential helpers, so ssh itself
//!   is never attempted. With no known token, or no HTTPS support in this
//!   build of libgit2, real git runs.
//! - **Source refs.** A short source name (`feature`) is resolved against the
//!   remote's advertised refs with git's own abbreviation rules, and a source
//!   the remote lacks fails with git's exact `couldn't find remote ref`
//!   message and exit code, which callers parse.
//! - **Tracking refs.** git also updates `refs/remotes/<remote>/<branch>` for
//!   a fetched branch when the remote's configured fetch refspec maps it; that
//!   update is added explicitly.
//! - **Refs are written directly**, never through libgit2's `update_tips`,
//!   which truncates `FETCH_HEAD` regardless of its flags. A destination
//!   branch some worktree has checked out is left to git, which refuses it.
//! - **Anything unusual** falls back to real git: flags other than
//!   `--no-write-fetch-head`, a refspec without `+`, wildcards, negative or
//!   object-id refspecs, shallow or partial clones, `uploadpack`/`vcs`/
//!   `promisor` remote settings, `fetch.fsckObjects`, a `reference-transaction`
//!   hook, config injected through `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS`,
//!   and `RALPHUS_GIT_INPROC=0`.
//!
//! Two things git does are not reproduced, because the daemon only reads the
//! refs it fetched: tags pointing into the fetched history are not followed,
//! and submodules are not fetched recursively (a later checkout that needs
//! submodule commits fetches them itself).
//!
//! A failed in-process attempt (network, authentication) falls back to real
//! git and keeps that URL on real git for [`FAILURE_BACKOFF`], so a remote
//! libgit2 cannot reach costs one failed attempt rather than one per poll.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::forge::ForgeKind;

/// How long a URL whose in-process fetch failed stays on real git.
const FAILURE_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// How long a token recorded by [`remember_token`] stays usable.
const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);

/// The longest an in-process fetch may run, matching the subprocess timeout
/// in `vcs.rs`.
const FETCH_DEADLINE: Duration = Duration::from_secs(60);

/// git's abbreviation rules for a ref name, in priority order
/// (`ref_rev_parse_rules` in git's `refs.c`).
const REF_RULES: &[(&str, &str)] = &[
    ("", ""),
    ("refs/", ""),
    ("refs/tags/", ""),
    ("refs/heads/", ""),
    ("refs/remotes/", ""),
    ("refs/remotes/", "/HEAD"),
];

/// `(host, repo path)` -> `(forge kind, token, when recorded)`.
type TokenHints = HashMap<(String, String), (ForgeKind, String, Instant)>;
static TOKEN_HINTS: Mutex<Option<TokenHints>> = Mutex::new(None);

/// Fetch URL -> when its last in-process attempt failed.
static FAILED_URLS: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Records the token a forge client resolved for the repository at
/// `host`/`path`, so an in-process fetch of that same repository can
/// authenticate as the daemon already does for its REST calls.
pub fn remember_token(kind: ForgeKind, host: &str, path: &str, token: &str) {
    if token.trim().is_empty() {
        return;
    }
    TOKEN_HINTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(
            (host.to_ascii_lowercase(), path.to_string()),
            (kind, token.to_string(), Instant::now()),
        );
}

fn token_hint(host: &str, path: &str) -> Option<(ForgeKind, String)> {
    let hints = TOKEN_HINTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (kind, token, at) = hints
        .as_ref()?
        .get(&(host.to_ascii_lowercase(), path.to_string()))?;
    (at.elapsed() < TOKEN_TTL).then(|| (*kind, token.clone()))
}

fn forget_token(host: &str, path: &str) {
    if let Some(hints) = TOKEN_HINTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        hints.remove(&(host.to_ascii_lowercase(), path.to_string()));
    }
}

fn recently_failed(url: &str) -> bool {
    FAILED_URLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|m| m.get(url))
        .is_some_and(|at| at.elapsed() < FAILURE_BACKOFF)
}

fn note_failure(url: &str, error: &str) {
    let first = FAILED_URLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(url.to_string(), Instant::now())
        .is_none_or(|at| at.elapsed() >= FAILURE_BACKOFF);
    if first {
        // ralphus[ignore-rlog-pair]: low-level transport switch with no Store; the git fallback keeps every caller's behavior unchanged
        crate::rlog!(
            WARNING,
            "ralphus [git] in-process fetch from {} failed ({error}); using git for it for the next {}s",
            redact(url),
            FAILURE_BACKOFF.as_secs()
        );
    }
}

/// `url` without any credentials it embeds.
fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

/// One `+<src>:<dst>` refspec from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Refspec<'a> {
    src: &'a str,
    dst: &'a str,
}

/// `fetch --no-write-fetch-head <remote> +<src>:<dst>...`, or `None` for any
/// other shape.
fn parse<'a>(args: &[&'a str]) -> Option<(&'a str, Vec<Refspec<'a>>)> {
    let ["fetch", "--no-write-fetch-head", remote, specs @ ..] = args else {
        return None;
    };
    if remote.is_empty() || remote.starts_with('-') || specs.is_empty() {
        return None;
    }
    let specs = specs
        .iter()
        .map(|spec| {
            let (src, dst) = spec.strip_prefix('+')?.split_once(':')?;
            let plain = |s: &str| {
                !s.is_empty()
                    && !s.contains(['*', '^', ':', '~', '?', '[', '\\', ' '])
                    && !s.contains("..")
            };
            (plain(src) && plain(dst) && dst.starts_with("refs/") && !is_object_id(src))
                .then_some(Refspec { src, dst })
        })
        .collect::<Option<Vec<_>>>()?;
    Some((remote, specs))
}

fn is_object_id(s: &str) -> bool {
    s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The advertised ref `name` resolves to under git's abbreviation rules: the
/// match from the earliest rule wins.
fn resolve_source<'a>(name: &str, advertised: &'a [String]) -> Option<&'a str> {
    REF_RULES.iter().find_map(|(prefix, suffix)| {
        let full = format!("{prefix}{name}{suffix}");
        advertised.iter().find(|r| **r == full).map(String::as_str)
    })
}

/// How to reach the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Transport {
    /// A local repository path or `file://` URL; no credentials.
    Local(String),
    /// A forge repository over HTTPS with its token.
    Https {
        url: String,
        kind: ForgeKind,
        host: String,
        path: String,
        token: String,
    },
}

impl Transport {
    fn url(&self) -> &str {
        match self {
            Transport::Local(url) | Transport::Https { url, .. } => url,
        }
    }
}

/// How `remote_url` is fetched in-process, or `None` to leave it to git.
fn transport_for(
    root: &Path,
    remote_url: &str,
    token_for: impl Fn(ForgeKind, &str, &str) -> Option<String>,
) -> Option<Transport> {
    let url = remote_url.trim();
    if url.starts_with("file://") || is_local_path(root, url) {
        return Some(Transport::Local(url.to_string()));
    }
    if !git2::Version::get().https() {
        return None;
    }
    let (host, path) = ralphus_core::git_remote::parse_remote_url(url)?;
    // An ssh port says nothing about where the forge serves HTTPS.
    if !url.starts_with("https://") && host.contains(':') {
        return None;
    }
    let cfg = crate::config::resolve_forge(root);
    let kind = cfg
        .kind
        .as_deref()
        .and_then(ForgeKind::parse)
        .or_else(|| ForgeKind::from_host(&host))?;
    let token = token_for(kind, &host, &path)?;
    let https_url = if url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{host}/{path}.git")
    };
    Some(Transport::Https {
        url: https_url,
        kind,
        host,
        path,
        token,
    })
}

fn is_local_path(root: &Path, url: &str) -> bool {
    if url.contains("://") || ralphus_core::git_remote::parse_remote_url(url).is_some() {
        return false;
    }
    let path = Path::new(url);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    path.is_dir()
}

/// The token for the repository at `host`/`path`: the one its forge client
/// last resolved, else the daemon's own env var or forge CLI login.
fn token_for(kind: ForgeKind, host: &str, path: &str, token_env: Option<&str>) -> Option<String> {
    token_hint(host, path)
        .filter(|(hinted, _)| *hinted == kind)
        .map(|(_, token)| token)
        .or_else(|| crate::forge::daemon_forge_token(kind, host, token_env))
}

/// Whether anything in `repo`'s setup makes libgit2's fetch differ from git's.
fn repo_needs_real_git(repo: &git2::Repository, remote: &str) -> bool {
    if repo.is_shallow() {
        return true;
    }
    let Ok(cfg) = repo.config() else {
        return true;
    };
    let set = |key: &str| cfg.get_entry(key).is_ok();
    let truthy = |key: &str| cfg.get_bool(key).unwrap_or(false);
    if set("extensions.partialclone")
        || set(&format!("remote.{remote}.uploadpack"))
        || set(&format!("remote.{remote}.vcs"))
        || truthy(&format!("remote.{remote}.promisor"))
        || truthy("fetch.fsckobjects")
        || truthy("transfer.fsckobjects")
    {
        return true;
    }
    has_reference_transaction_hook(repo, &cfg)
}

fn has_reference_transaction_hook(repo: &git2::Repository, cfg: &git2::Config) -> bool {
    let hooks: PathBuf = match cfg.get_path("core.hookspath") {
        Ok(path) if path.is_absolute() => path,
        // A relative hooks path is resolved against the directory git runs
        // in, which this module does not reproduce.
        Ok(_) => return true,
        Err(_) => common_dir(repo.path()).join("hooks"),
    };
    hooks.join("reference-transaction").exists()
}

/// The repository's common git dir: a linked worktree's git dir names it in
/// its `commondir` file; any other git dir is its own.
fn common_dir(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => git_dir.join(rel.trim()),
        Err(_) => git_dir.to_path_buf(),
    }
}

/// The `(source, destination)` ref pairs to fetch: each command-line refspec
/// with its source fully qualified, plus the remote-tracking ref git updates
/// for it. `Err` names the first source the remote does not have; `Ok(None)`
/// means a configured refspec git would apply without force, which is left
/// to git.
fn plan_refspecs(
    specs: &[Refspec<'_>],
    advertised: &[String],
    configured: &[(bool, String, String)],
) -> Result<Option<Vec<(String, String)>>, String> {
    let mut planned: Vec<(String, String)> = Vec::new();
    let push = |src: &str, dst: &str, planned: &mut Vec<(String, String)>| {
        if !planned.iter().any(|(_, d)| d == dst) {
            planned.push((src.to_string(), dst.to_string()));
        }
    };
    for spec in specs {
        let Some(src) = resolve_source(spec.src, advertised) else {
            return Err(spec.src.to_string());
        };
        push(src, spec.dst, &mut planned);
        for (force, cfg_src, cfg_dst) in configured {
            let Some(tracking) = map_refspec(cfg_src, cfg_dst, src) else {
                continue;
            };
            if !force {
                return Ok(None);
            }
            push(src, &tracking, &mut planned);
        }
    }
    Ok(Some(planned))
}

/// Where configured refspec `src:dst` maps `name`, if it matches.
fn map_refspec(src: &str, dst: &str, name: &str) -> Option<String> {
    match (src.split_once('*'), dst.split_once('*')) {
        (Some((sp, ss)), Some((dp, ds))) => {
            let middle = name.strip_prefix(sp)?.strip_suffix(ss)?;
            Some(format!("{dp}{middle}{ds}"))
        }
        (None, None) => (src == name).then(|| dst.to_string()),
        _ => None,
    }
}

/// The result of an in-process attempt.
enum Attempt {
    Done(Output),
    /// Leave this fetch to real git.
    Fallback,
}

/// Runs `args` as an in-process fetch when it is the shape this module
/// reproduces; `None` means run real git.
pub fn try_fetch(root: &Path, args: &[&str]) -> Option<Output> {
    // Every git call passes through here; only a fetch pays for config.
    parse(args)?;
    let token_env = crate::config::resolve_forge(root).token_env;
    try_fetch_with(root, args, |kind, host, path| {
        token_for(kind, host, path, token_env.as_deref())
    })
}

fn try_fetch_with(
    root: &Path,
    args: &[&str],
    token_for: impl Fn(ForgeKind, &str, &str) -> Option<String>,
) -> Option<Output> {
    let (remote_name, specs) = parse(args)?;
    if !ralphus_runner::git_inproc::enabled() || ralphus_runner::git_inproc::config_env_override() {
        return None;
    }
    let repo = git2::Repository::open(root).ok()?;
    if repo_needs_real_git(&repo, remote_name) {
        return None;
    }
    let named = repo.find_remote(remote_name).ok()?;
    let mut configured = Vec::new();
    for spec in named.refspecs() {
        if spec.direction() != git2::Direction::Fetch {
            continue;
        }
        let (Some(src), Some(dst)) = (spec.src(), spec.dst()) else {
            return None;
        };
        if src.starts_with('^') {
            return None;
        }
        configured.push((spec.is_force(), src.to_string(), dst.to_string()));
    }
    let transport = transport_for(root, named.url()?, token_for)?;
    if recently_failed(transport.url()) {
        return None;
    }
    let started = Instant::now();
    match fetch(root, &repo, remote_name, &transport, &specs, &configured) {
        Ok(Attempt::Done(output)) => {
            // ralphus[ignore-rlog-pair]: low-level transport detail with no Store; callers record the outcome of the work the fetch serves
            crate::rlog!(
                DEBUG,
                "ralphus [git] fetch {remote_name} ({} refspec(s)) answered in-process from {} in {}ms: exit {}",
                specs.len(),
                redact(transport.url()),
                started.elapsed().as_millis(),
                output.status.code().unwrap_or(-1)
            );
            Some(output)
        }
        Ok(Attempt::Fallback) => None,
        Err(e) => {
            // The forge client records the token again on its next resolve,
            // so a token that has gone bad is not offered twice.
            if let Transport::Https { host, path, .. } = &transport {
                forget_token(host, path);
            }
            note_failure(transport.url(), &e);
            None
        }
    }
}

fn callbacks(transport: &Transport, deadline: Instant) -> git2::RemoteCallbacks<'static> {
    let mut cbs = git2::RemoteCallbacks::new();
    if let Transport::Https { kind, token, .. } = transport {
        let user = forge_git_user(*kind);
        let token = token.clone();
        let mut offered = false;
        cbs.credentials(move |_url, _user, allowed| {
            // libgit2 asks again after a rejection; a second offer of the
            // same token would loop.
            if offered || !allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
                return Err(git2::Error::from_str("forge token was not accepted"));
            }
            offered = true;
            git2::Cred::userpass_plaintext(user, &token)
        });
    }
    cbs.transfer_progress(move |_| Instant::now() < deadline);
    cbs.sideband_progress(move |_| Instant::now() < deadline);
    cbs
}

/// The refs `transport`'s remote advertises and the commits they point at,
/// peeled tag entries (`^{}`) left out, or `None` when they cannot be listed
/// the way git's upload-pack would list them.
///
/// The list is read here rather than through libgit2's `Remote::list`, which
/// in this git2 version builds a slice from a null pointer whenever a remote
/// advertises no refs at all (an empty repository) -- undefined behavior that
/// aborts the process in debug builds.
fn list_refs(
    root: &Path,
    transport: &Transport,
) -> Result<Option<Vec<(String, git2::Oid)>>, String> {
    match transport {
        Transport::Local(url) => local_refs(&local_path(root, url)),
        Transport::Https {
            url, kind, token, ..
        } => https_refs(url, *kind, token).map(Some),
    }
}

fn local_path(root: &Path, url: &str) -> PathBuf {
    let raw = url.strip_prefix("file://").unwrap_or(url);
    // `file:///C:/x` names the drive path `C:/x`.
    let raw = match raw.strip_prefix('/') {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => raw,
    };
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// What `git upload-pack` would advertise for the repository at `path`:
/// `HEAD` when it resolves, then every ref.
fn local_refs(path: &Path) -> Result<Option<Vec<(String, git2::Oid)>>, String> {
    let remote = git2::Repository::open(path).map_err(|e| e.message().to_string())?;
    let cfg = remote.config().map_err(|e| e.message().to_string())?;
    if cfg.get_entry("uploadpack.hiderefs").is_ok() || cfg.get_entry("transfer.hiderefs").is_ok() {
        return Ok(None);
    }
    let mut out = Vec::new();
    if let Some(oid) = remote.head().ok().and_then(|h| h.target()) {
        out.push(("HEAD".to_string(), oid));
    }
    for reference in remote.references().map_err(|e| e.message().to_string())? {
        let reference = reference.map_err(|e| e.message().to_string())?;
        let Some(name) = reference.name() else {
            return Ok(None);
        };
        if let Some(oid) = reference.resolve().ok().and_then(|r| r.target()) {
            out.push((name.to_string(), oid));
        }
    }
    Ok(Some(out))
}

/// The ref advertisement from a smart-HTTP remote's
/// `info/refs?service=git-upload-pack`.
fn https_refs(url: &str, kind: ForgeKind, token: &str) -> Result<Vec<(String, git2::Oid)>, String> {
    use base64::Engine as _;
    let endpoint = format!(
        "{}/info/refs?service=git-upload-pack",
        url.trim_end_matches('/')
    );
    let credentials = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{token}", forge_git_user(kind)));
    let response = crate::forge::bounded_http_agent(Duration::from_secs(30))
        .get(&endpoint)
        .set("Authorization", &format!("Basic {credentials}"))
        .set("User-Agent", "git/2.0 (ralphus)")
        .call()
        .map_err(|e| format!("listing refs: {}", redact(&e.to_string())))?;
    if response.content_type() != "application/x-git-upload-pack-advertisement" {
        return Err(format!(
            "listing refs: unexpected content type {:?}",
            response.content_type()
        ));
    }
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut response.into_reader(), &mut body)
        .map_err(|e| format!("listing refs: {e}"))?;
    parse_advertisement(&body)
}

/// The refs in a protocol-v0 smart-HTTP ref advertisement (pkt-lines).
fn parse_advertisement(body: &[u8]) -> Result<Vec<(String, git2::Oid)>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 4 <= body.len() {
        let len = std::str::from_utf8(&body[pos..pos + 4])
            .ok()
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            .ok_or("listing refs: malformed pkt-line length")?;
        if len == 0 {
            pos += 4;
            continue;
        }
        if len < 4 || pos + len > body.len() {
            return Err("listing refs: truncated pkt-line".to_string());
        }
        let line = &body[pos + 4..pos + len];
        pos += len;
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.split(|b| *b == 0).next().unwrap_or(line);
        let line = std::str::from_utf8(line).map_err(|_| "listing refs: non-UTF-8 ref name")?;
        if line.starts_with("# service=") || line == "version 1" {
            continue;
        }
        if line.starts_with("version ") {
            return Err(format!("listing refs: unsupported protocol {line:?}"));
        }
        let (oid, name) = line
            .split_once(' ')
            .ok_or("listing refs: malformed ref line")?;
        if name == "capabilities^{}" || name.ends_with("^{}") {
            continue;
        }
        let oid = git2::Oid::from_str(oid).map_err(|_| "listing refs: malformed object id")?;
        out.push((name.to_string(), oid));
    }
    Ok(out)
}

/// The username a forge accepts alongside a token for git over HTTPS.
fn forge_git_user(kind: ForgeKind) -> &'static str {
    match kind {
        ForgeKind::GitHub => "x-access-token",
        ForgeKind::GitLab => "oauth2",
    }
}

/// Downloads `specs` from the remote and writes each destination ref.
///
/// The refs are written here rather than through libgit2's `update_tips`,
/// which truncates `FETCH_HEAD` on every call: git leaves that file alone under
/// `--no-write-fetch-head`, and another git command may be reading it. Each
/// ref is set to the commit the listing advertised, once that commit is known
/// to be in the object database. Tags are not followed; nothing in the daemon
/// reads a fetched tag.
fn fetch(
    root: &Path,
    repo: &git2::Repository,
    remote_name: &str,
    transport: &Transport,
    specs: &[Refspec<'_>],
    configured: &[(bool, String, String)],
) -> Result<Attempt, String> {
    let deadline = Instant::now() + FETCH_DEADLINE;
    let Some(advertised) = list_refs(root, transport)? else {
        return Ok(Attempt::Fallback);
    };
    let names: Vec<String> = advertised.iter().map(|(n, _)| n.clone()).collect();
    let planned = match plan_refspecs(specs, &names, configured) {
        Ok(Some(planned)) => planned,
        Ok(None) => return Ok(Attempt::Fallback),
        Err(missing) => {
            return Ok(Attempt::Done(failure_output(&format!(
                "fatal: couldn't find remote ref {missing}\n"
            ))));
        }
    };
    // git refuses to fetch into a branch a worktree has checked out.
    if planned.iter().any(|(_, dst)| checked_out(repo, dst)) {
        return Ok(Attempt::Fallback);
    }
    let tips: HashMap<String, git2::Oid> = advertised.into_iter().collect();
    let odb = repo.odb().map_err(|e| e.message().to_string())?;
    // Nothing to download when every advertised commit is already here.
    if planned
        .iter()
        .any(|(src, _)| tips.get(src).is_none_or(|oid| !odb.exists(*oid)))
    {
        let Some(mut remote) =
            unrewritten_remote(repo, transport).map_err(|e| e.message().to_string())?
        else {
            return Ok(Attempt::Fallback);
        };
        let wanted: Vec<String> = planned
            .iter()
            .map(|(src, dst)| format!("+{src}:{dst}"))
            .collect();
        let mut opts = git2::FetchOptions::new();
        opts.remote_callbacks(callbacks(transport, deadline))
            .proxy_options(proxy())
            .update_fetchhead(false)
            .download_tags(git2::AutotagOption::None);
        remote
            .download(&wanted, Some(&mut opts))
            .map_err(|e| e.message().to_string())?;
        remote.disconnect().map_err(|e| e.message().to_string())?;
    }
    let mut updates = Vec::with_capacity(planned.len());
    for (src, dst) in &planned {
        // A commit missing after the download means the remote moved between
        // the listing and the download; git's own fetch answers that.
        match tips.get(src) {
            Some(oid) if odb.exists(*oid) => updates.push((dst, *oid)),
            _ => return Ok(Attempt::Fallback),
        }
    }
    let message = format!("fetch {remote_name}: storing head");
    for (dst, oid) in updates {
        repo.reference(dst, oid, true, &message)
            .map_err(|e| e.message().to_string())?;
    }
    Ok(Attempt::Done(
        crate::vcs_refs::success_output(String::new()),
    ))
}

/// An anonymous remote for `transport`'s URL that libgit2 has not rewritten.
///
/// libgit2 applies `url.<base>.insteadOf` to an anonymous remote, and a
/// common rule rewrites `https://github.com/` to ssh, which this build cannot
/// speak. Such a rule matches by prefix, so the same HTTPS URL with the port
/// spelled out is tried next; if that is rewritten too, the fetch is left to
/// git.
fn unrewritten_remote<'r>(
    repo: &'r git2::Repository,
    transport: &Transport,
) -> Result<Option<git2::Remote<'r>>, git2::Error> {
    let url = transport.url();
    let mut candidates = vec![url.to_string()];
    if let Transport::Https { .. } = transport {
        if let Some(explicit) = with_default_https_port(url) {
            candidates.push(explicit);
        }
    }
    for candidate in candidates {
        let remote = repo.remote_anonymous(&candidate)?;
        if remote.url() == Some(candidate.as_str()) {
            return Ok(Some(remote));
        }
    }
    Ok(None)
}

/// `https://host/path` as `https://host:443/path`, or `None` when the URL
/// already names a port.
fn with_default_https_port(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let (authority, path) = rest.split_once('/')?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.contains(':') {
        return None;
    }
    Some(format!("https://{authority}:443/{path}"))
}

/// Whether `refname` is the branch checked out in the repository or any of
/// its linked worktrees.
fn checked_out(repo: &git2::Repository, refname: &str) -> bool {
    if !refname.starts_with("refs/heads/") {
        return false;
    }
    let common = common_dir(repo.path());
    let points_here = |head: &Path| {
        std::fs::read_to_string(head)
            .ok()
            .and_then(|s| s.trim().strip_prefix("ref:").map(|r| r.trim() == refname))
            .unwrap_or(false)
    };
    if points_here(&common.join("HEAD")) {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| points_here(&entry.path().join("HEAD")))
}

fn proxy() -> git2::ProxyOptions<'static> {
    let mut proxy = git2::ProxyOptions::new();
    proxy.auto();
    proxy
}

fn failure_output(stderr: &str) -> Output {
    let mut output = crate::vcs_refs::answer_output(String::new(), 128);
    output.stderr = stderr.as_bytes().to_vec();
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_the_forced_explicit_refspec_shape() {
        let (remote, specs) =
            parse(&["fetch", "--no-write-fetch-head", "origin", "+a:refs/x/a"]).unwrap();
        assert_eq!(remote, "origin");
        assert_eq!(
            specs,
            vec![Refspec {
                src: "a",
                dst: "refs/x/a"
            }]
        );
        for args in [
            &["fetch", "origin", "+a:refs/x/a"][..],
            &["fetch", "--no-write-fetch-head", "origin"],
            &["fetch", "--no-write-fetch-head", "origin", "a:refs/x/a"],
            &["fetch", "--no-write-fetch-head", "origin", "+a"],
            &["fetch", "--no-write-fetch-head", "origin", "+a:b"],
            &[
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+refs/heads/*:refs/x/*",
            ],
            &[
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+^refs/heads/a:refs/x/a",
            ],
            &[
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+deadbeef:refs/x/a",
            ],
            &["fetch", "--no-write-fetch-head", "-q", "+a:refs/x/a"],
            &[
                "fetch",
                "--no-write-fetch-head",
                "--prune",
                "origin",
                "+a:refs/x/a",
            ],
        ] {
            assert_eq!(parse(args), None, "{args:?}");
        }
    }

    #[test]
    fn resolves_sources_with_gits_abbreviation_rules() {
        let advertised: Vec<String> = [
            "HEAD",
            "refs/heads/main",
            "refs/heads/v1",
            "refs/tags/v1",
            "refs/heads/feature",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(resolve_source("main", &advertised), Some("refs/heads/main"));
        assert_eq!(
            resolve_source("refs/heads/main", &advertised),
            Some("refs/heads/main")
        );
        // A tag beats a branch of the same name, as in git.
        assert_eq!(resolve_source("v1", &advertised), Some("refs/tags/v1"));
        assert_eq!(resolve_source("HEAD", &advertised), Some("HEAD"));
        assert_eq!(resolve_source("gone", &advertised), None);
    }

    #[test]
    fn plans_the_opportunistic_tracking_update_and_reports_the_first_missing_source() {
        let advertised: Vec<String> = ["refs/heads/a", "refs/heads/b"].map(String::from).to_vec();
        let configured = vec![(
            true,
            "refs/heads/*".to_string(),
            "refs/remotes/origin/*".to_string(),
        )];
        let specs = [
            Refspec {
                src: "a",
                dst: "refs/ralphus/sync/pr-1",
            },
            Refspec {
                src: "refs/heads/b",
                dst: "refs/remotes/origin/b",
            },
        ];
        let pair = |s: &str, d: &str| (s.to_string(), d.to_string());
        assert_eq!(
            plan_refspecs(&specs, &advertised, &configured),
            Ok(Some(vec![
                pair("refs/heads/a", "refs/ralphus/sync/pr-1"),
                pair("refs/heads/a", "refs/remotes/origin/a"),
                pair("refs/heads/b", "refs/remotes/origin/b"),
            ]))
        );
        let missing = [
            Refspec {
                src: "a",
                dst: "refs/x/a",
            },
            Refspec {
                src: "gone",
                dst: "refs/x/gone",
            },
            Refspec {
                src: "also-gone",
                dst: "refs/x/also-gone",
            },
        ];
        assert_eq!(
            plan_refspecs(&missing, &advertised, &configured),
            Err("gone".to_string())
        );
        let unforced = vec![(
            false,
            "refs/heads/*".to_string(),
            "refs/remotes/origin/*".to_string(),
        )];
        assert_eq!(plan_refspecs(&specs[..1], &advertised, &unforced), Ok(None));
    }

    #[test]
    fn rewrites_a_forge_ssh_remote_to_https_only_with_a_token() {
        if !git2::Version::get().https() {
            return;
        }
        let root = Path::new(".");
        let with_token = |_: ForgeKind, _: &str, _: &str| Some("tok".to_string());
        let no_token = |_: ForgeKind, _: &str, _: &str| None;
        assert_eq!(
            transport_for(root, "git@github.com:acme/widget.git", with_token)
                .map(|t| t.url().to_string()),
            Some("https://github.com/acme/widget.git".to_string())
        );
        assert_eq!(
            transport_for(
                root,
                "ssh://git@gitlab.com/group/sub/widget.git",
                with_token
            )
            .map(|t| t.url().to_string()),
            Some("https://gitlab.com/group/sub/widget.git".to_string())
        );
        assert_eq!(
            transport_for(root, "https://github.com/acme/widget", with_token)
                .map(|t| t.url().to_string()),
            Some("https://github.com/acme/widget".to_string())
        );
        assert_eq!(
            transport_for(root, "git@github.com:acme/widget.git", no_token),
            None
        );
        assert_eq!(
            transport_for(
                root,
                "ssh://git@github.com:2222/acme/widget.git",
                with_token
            ),
            None
        );
        assert_eq!(
            transport_for(root, "git@example.org:acme/widget.git", with_token),
            None
        );
    }

    fn pkt(line: &str) -> String {
        format!("{:04x}{line}", line.len() + 4)
    }

    #[test]
    fn parses_a_smart_http_ref_advertisement() {
        let a = "1".repeat(40);
        let b = "2".repeat(40);
        let body = [
            pkt("# service=git-upload-pack\n"),
            "0000".to_string(),
            pkt(&format!(
                "{a} HEAD\0multi_ack side-band-64k symref=HEAD:refs/heads/main\n"
            )),
            pkt(&format!("{a} refs/heads/main\n")),
            pkt(&format!("{b} refs/tags/v1\n")),
            pkt(&format!("{a} refs/tags/v1^{{}}\n")),
            "0000".to_string(),
        ]
        .concat();
        let oid = |s: &str| git2::Oid::from_str(s).unwrap();
        assert_eq!(
            parse_advertisement(body.as_bytes()),
            Ok(vec![
                ("HEAD".to_string(), oid(&a)),
                ("refs/heads/main".to_string(), oid(&a)),
                ("refs/tags/v1".to_string(), oid(&b)),
            ])
        );
    }

    #[test]
    fn an_empty_repositorys_advertisement_has_no_refs() {
        let zero = "0".repeat(40);
        let body = [
            pkt("# service=git-upload-pack\n"),
            "0000".to_string(),
            pkt(&format!("{zero} capabilities^{{}}\0side-band-64k\n")),
            "0000".to_string(),
        ]
        .concat();
        assert_eq!(parse_advertisement(body.as_bytes()), Ok(Vec::new()));
    }

    #[test]
    fn a_protocol_v2_or_malformed_advertisement_is_rejected() {
        let v2 = [pkt("version 2\n"), pkt("ls-refs\n"), "0000".to_string()].concat();
        assert!(parse_advertisement(v2.as_bytes()).is_err());
        assert!(parse_advertisement(b"zzzzjunk").is_err());
        assert!(parse_advertisement(b"00ffshort").is_err());
    }

    #[test]
    fn local_paths_resolve_like_git() {
        let root = Path::new("C:/work/repo");
        assert_eq!(
            local_path(root, "file:///C:/remotes/up.git"),
            PathBuf::from("C:/remotes/up.git")
        );
        assert_eq!(local_path(root, "../up.git"), root.join("../up.git"));
    }

    #[test]
    fn redacts_credentials_from_logged_urls() {
        assert_eq!(
            redact("https://user:secret@github.com/acme/w.git"),
            "https://github.com/acme/w.git"
        );
        assert_eq!(
            redact("https://github.com/acme/w.git"),
            "https://github.com/acme/w.git"
        );
    }

    /// Real-git parity: the same fetch run through git and in-process leaves
    /// the same refs behind, and a missing source fails the same way.
    mod parity {
        use super::*;
        use std::process::Command;

        fn git(dir: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn real_git_output(dir: &Path, args: &[&str]) -> Output {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("git runs")
        }

        /// An upstream repo with `main` and `feature`, and two independent
        /// clones of it whose `origin` is the upstream path.
        fn temp_dir() -> PathBuf {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ralphus-fetch-inproc-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            path
        }

        struct Tmp(PathBuf);
        impl Tmp {
            fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Tmp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn fixture() -> (Tmp, PathBuf, PathBuf, PathBuf) {
            let tmp = Tmp(temp_dir());
            let upstream = tmp.path().join("upstream");
            std::fs::create_dir(&upstream).unwrap();
            git(&upstream, &["init", "-q", "-b", "main"]);
            std::fs::write(upstream.join("f"), "1").unwrap();
            git(&upstream, &["add", "f"]);
            git(&upstream, &["commit", "-q", "-m", "one"]);
            git(&upstream, &["checkout", "-q", "-b", "feature"]);
            std::fs::write(upstream.join("f"), "2").unwrap();
            git(&upstream, &["commit", "-q", "-am", "two"]);
            git(&upstream, &["checkout", "-q", "main"]);
            let up = upstream.to_string_lossy().replace('\\', "/");
            let a = tmp.path().join("a");
            let b = tmp.path().join("b");
            for clone in [&a, &b] {
                git(
                    tmp.path(),
                    &[
                        "clone",
                        "-q",
                        &up,
                        &clone.to_string_lossy().replace('\\', "/"),
                    ],
                );
            }
            (tmp, upstream, a, b)
        }

        fn refs(dir: &Path) -> String {
            git(dir, &["for-each-ref", "--format=%(refname) %(objectname)"])
        }

        fn no_token(_: ForgeKind, _: &str, _: &str) -> Option<String> {
            None
        }

        #[test]
        fn fetch_leaves_the_same_refs_as_git() {
            let (_tmp, upstream, a, b) = fixture();
            std::fs::write(upstream.join("f"), "3").unwrap();
            git(&upstream, &["commit", "-q", "-am", "three"]);
            let args = [
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+feature:refs/ralphus/sync/pr-1",
                "+refs/heads/main:refs/heads/main-copy",
            ];
            let real = real_git_output(&a, &args);
            assert!(real.status.success());
            let inproc = try_fetch_with(&b, &args, no_token).expect("answered in-process");
            assert!(inproc.status.success());
            assert_eq!(refs(&a), refs(&b));
            assert!(refs(&b).contains("refs/remotes/origin/feature"));
        }

        #[test]
        fn a_missing_source_fails_exactly_like_git() {
            let (_tmp, _upstream, a, b) = fixture();
            let args = [
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+feature:refs/x/feature",
                "+gone:refs/x/gone",
            ];
            let real = real_git_output(&a, &args);
            let inproc = try_fetch_with(&b, &args, no_token).expect("answered in-process");
            assert_eq!(inproc.status.code(), real.status.code());
            assert_eq!(
                String::from_utf8_lossy(&inproc.stderr).trim(),
                String::from_utf8_lossy(&real.stderr).trim()
            );
            assert_eq!(refs(&a), refs(&b), "git fetched nothing, and neither do we");
        }

        #[test]
        fn a_forced_non_fast_forward_moves_the_ref() {
            let (_tmp, upstream, _a, b) = fixture();
            let args = [
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+feature:refs/x/feature",
            ];
            assert!(
                try_fetch_with(&b, &args, no_token)
                    .unwrap()
                    .status
                    .success()
            );
            git(&upstream, &["checkout", "-q", "feature"]);
            git(&upstream, &["reset", "-q", "--hard", "main"]);
            std::fs::write(upstream.join("f"), "rewritten").unwrap();
            git(&upstream, &["commit", "-q", "-am", "rewritten"]);
            assert!(
                try_fetch_with(&b, &args, no_token)
                    .unwrap()
                    .status
                    .success()
            );
            assert_eq!(
                git(&b, &["rev-parse", "refs/x/feature"]),
                git(&upstream, &["rev-parse", "feature"])
            );
        }

        #[test]
        fn never_writes_or_truncates_fetch_head() {
            let (_tmp, _upstream, _a, b) = fixture();
            let fetch_head = b.join(".git/FETCH_HEAD");
            std::fs::write(&fetch_head, "left by another git command\n").unwrap();
            let args = [
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+feature:refs/x/f",
            ];
            assert!(
                try_fetch_with(&b, &args, no_token)
                    .unwrap()
                    .status
                    .success()
            );
            assert_eq!(
                std::fs::read_to_string(&fetch_head).unwrap(),
                "left by another git command\n"
            );
        }

        #[test]
        fn a_branch_checked_out_in_a_worktree_is_left_to_git() {
            let (tmp, _upstream, _a, b) = fixture();
            let wt = tmp.path().join("wt-checked-out");
            git(
                &b,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "local-feature",
                    &wt.to_string_lossy().replace('\\', "/"),
                ],
            );
            let into_checked_out = [
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+feature:refs/heads/local-feature",
            ];
            assert!(try_fetch_with(&b, &into_checked_out, no_token).is_none());
            let into_main_head = [
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+feature:refs/heads/main",
            ];
            assert!(try_fetch_with(&b, &into_main_head, no_token).is_none());
        }

        #[test]
        fn an_empty_remote_fails_like_git_instead_of_crashing() {
            let (tmp, _upstream, a, b) = fixture();
            let empty = tmp.path().join("empty.git");
            std::fs::create_dir(&empty).unwrap();
            git(&empty, &["init", "-q", "--bare"]);
            let url = empty.to_string_lossy().replace('\\', "/");
            for clone in [&a, &b] {
                git(clone, &["remote", "add", "empty", &url]);
            }
            let args = [
                "fetch",
                "--no-write-fetch-head",
                "empty",
                "+main:refs/x/main",
            ];
            let real = real_git_output(&a, &args);
            let inproc = try_fetch_with(&b, &args, no_token).expect("answered in-process");
            assert_eq!(inproc.status.code(), real.status.code());
            assert_eq!(
                String::from_utf8_lossy(&inproc.stderr).trim(),
                String::from_utf8_lossy(&real.stderr).trim()
            );
        }

        /// A real GitHub fetch over HTTPS, authenticated with the daemon's
        /// token chain (`RALPHUS_GITHUB_TOKEN` or the `gh` login), checked
        /// against `git ls-remote`. Needs the network and a token, so it only
        /// runs on request:
        /// `cargo nextest run -p ralphus-daemon --lib live_https_fetch --run-ignored only`.
        #[test]
        #[ignore = "needs network access and a GitHub token"]
        fn live_https_fetch_from_github_matches_ls_remote() {
            if !git2::Version::get().https() {
                return;
            }
            let remote = std::env::var("RALPHUS_LIVE_FETCH_REMOTE")
                .unwrap_or_else(|_| "git@github.com:rust-lang/git2-rs.git".to_string());
            let tmp = Tmp(temp_dir());
            let repo = tmp.path().join("repo");
            std::fs::create_dir(&repo).unwrap();
            git(&repo, &["init", "-q"]);
            git(&repo, &["remote", "add", "origin", &remote]);
            let args = [
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+HEAD:refs/x/head",
            ];
            let out = try_fetch_with(&repo, &args, |kind, host, _| {
                crate::forge::daemon_forge_token(kind, host, None)
            })
            .expect("answered in-process");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let fetched = git(&repo, &["rev-parse", "refs/x/head"]);
            let listed = git(&repo, &["ls-remote", "origin", "HEAD"]);
            assert_eq!(listed.split_whitespace().next(), Some(fetched.as_str()));
        }

        #[test]
        fn an_insteadof_rule_on_the_https_url_is_sidestepped_or_left_to_git() {
            assert_eq!(
                with_default_https_port("https://github.com/acme/w.git").as_deref(),
                Some("https://github.com:443/acme/w.git")
            );
            assert_eq!(with_default_https_port("https://h:8443/acme/w.git"), None);
            let (_tmp, _upstream, _a, b) = fixture();
            let repo = git2::Repository::open(&b).unwrap();
            let transport = Transport::Https {
                url: "https://github.com/acme/w.git".to_string(),
                kind: ForgeKind::GitHub,
                host: "github.com".to_string(),
                path: "acme/w".to_string(),
                token: "tok".to_string(),
            };
            git(
                &b,
                &[
                    "config",
                    "url.ssh://git@github.com/.insteadOf",
                    "https://github.com/",
                ],
            );
            let remote = unrewritten_remote(&repo, &transport).unwrap().unwrap();
            assert_eq!(remote.url().unwrap(), "https://github.com:443/acme/w.git");
            git(
                &b,
                &[
                    "config",
                    "url.ssh://git@github.com.insteadOf",
                    "https://github.com",
                ],
            );
            let repo = git2::Repository::open(&b).unwrap();
            assert!(unrewritten_remote(&repo, &transport).unwrap().is_none());
        }

        #[test]
        fn a_reference_transaction_hook_leaves_the_fetch_to_git() {
            let (_tmp, _upstream, _a, b) = fixture();
            std::fs::write(b.join(".git/hooks/reference-transaction"), "#!/bin/sh\n").unwrap();
            let args = [
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+feature:refs/x/f",
            ];
            assert!(try_fetch_with(&b, &args, no_token).is_none());
        }

        #[test]
        fn fetches_into_a_linked_worktree_like_git() {
            let (tmp, _upstream, a, b) = fixture();
            let wt_a = tmp.path().join("wt-a");
            let wt_b = tmp.path().join("wt-b");
            git(
                &a,
                &[
                    "worktree",
                    "add",
                    "-q",
                    &wt_a.to_string_lossy().replace('\\', "/"),
                ],
            );
            git(
                &b,
                &[
                    "worktree",
                    "add",
                    "-q",
                    &wt_b.to_string_lossy().replace('\\', "/"),
                ],
            );
            let args = [
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+feature:refs/remotes/origin/feature",
            ];
            assert!(real_git_output(&wt_a, &args).status.success());
            assert!(
                try_fetch_with(&wt_b, &args, no_token)
                    .unwrap()
                    .status
                    .success()
            );
            assert_eq!(
                git(&a, &["rev-parse", "refs/remotes/origin/feature"]),
                git(&b, &["rev-parse", "refs/remotes/origin/feature"])
            );
        }
    }
}
