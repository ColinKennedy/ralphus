//! A directory *plus the machine it lives on* (RAL-185 Phase 3c).
//!
//! Everything in the review path used to take a bare `root: &Path`, which
//! answers "which folder" but not "which host". That was fine while every
//! review ran on the daemon's own box. Once a review can be assigned a machine
//! (**D3**), a path alone is ambiguous: the same project directory can belong
//! to one review running locally and another running on a build farm, so no
//! amount of inspecting the path reveals where its commands should run.
//!
//! [`Workspace`] carries both, and is passed down the call chain explicitly
//! rather than looked up from shared state. That choice is deliberate: a
//! lookup table keyed by path would need no signature changes at all, but a
//! stale or missing entry means work silently runs on the wrong machine — the
//! precise failure this whole feature exists to prevent, and one that leaves no
//! trace in the code explaining itself. Passing it makes every site the
//! compiler's problem instead of a debugging session's.
//!
//! ## The local path costs nothing
//!
//! A local `Workspace` calls the same free function it always did
//! ([`crate::guardian_merge::git`]) — same process spawn, same arguments, no
//! provider, no loopback, no serialization. A review that never mentions a
//! machine runs exactly the code it ran before this module existed. That is a
//! hard requirement, not an optimisation: local reviews are the common case and
//! must not pay for a capability they do not use.

use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(test)]
use crate::store::Store;

/// A working directory together with the machine it lives on.
///
/// `Debug` is hand-written rather than derived: the store handle it carries is
/// not `Debug`, and printing a whole `Store` in a diagnostic would be noise
/// anyway — what a reader wants is the path and the machine.
#[derive(Clone)]
pub struct Workspace {
    /// The directory, as named on `machine` (not necessarily on this host).
    root: PathBuf,
    /// The resolved machine, or `None` for the daemon's own host.
    machine: Option<String>,
    /// Needed only to resolve a remote machine's provider. `None` for a local
    /// workspace, which never consults the registry — keeping the local path
    /// free of any dependency it does not use.
    store: Option<crate::store_lock::StoreHandle>,
    /// For a review workspace on a remote machine: `(daemon-local project
    /// root, that project's repository on the machine)`. Paths the review
    /// code derives from its daemon-local project root (`worktree_dir`, the
    /// combined checkout, ...) are translated through it in [`Self::at`] and
    /// when resolving an absolute path. `None` everywhere else.
    path_map: Option<Arc<(PathBuf, PathBuf)>>,
    /// The path as the daemon names it, before any `path_map` translation:
    /// the registered project path config and project lookups are keyed by.
    /// Equal to `root` for every workspace that was not translated.
    local_root: PathBuf,
    /// For a remote workspace whose project is cloned over HTTPS: where its
    /// network git commands get a forge credential. `None` otherwise, and
    /// the machine's own git credentials are used.
    forge_credentials: Option<Arc<ForgeCredentials>>,
}

/// Where a remote workspace's network git commands (`push`, `fetch`, ...)
/// get a forge credential: the project's clone URL and the identity whose
/// token to use. The token is looked up once, on the first command that
/// needs it, and handed to that command only through its environment and an
/// inline `credential.helper` that answers for the clone URL's host alone --
/// never written to the machine's disk or a git config file.
pub struct ForgeCredentials {
    clone_url: String,
    identity: Option<String>,
    config_root: PathBuf,
    resolved: std::sync::OnceLock<Option<GitCredential>>,
}

/// A resolved credential for one forge host.
struct GitCredential {
    host: String,
    username: &'static str,
    token: String,
}

/// Git subcommands that may talk to a remote and so need a credential.
const NETWORK_GIT_COMMANDS: [&str; 4] = ["push", "fetch", "pull", "ls-remote"];

/// The `credential.helper` a remote network git command gets: answers only
/// a `get` for the host in `$RALPHUS_GIT_HOST`, from the environment.
const INLINE_CREDENTIAL_HELPER: &str = "!f() { test \"$1\" = get || exit 0; h=; \
     while IFS== read -r k v; do [ \"$k\" = host ] && h=$v; done; \
     [ \"$h\" = \"$RALPHUS_GIT_HOST\" ] || exit 0; \
     printf 'username=%s\\npassword=%s\\n' \"$RALPHUS_GIT_USERNAME\" \"$RALPHUS_GIT_PASSWORD\"; }; f";

impl ForgeCredentials {
    /// Credentials for `clone_url` as `identity` (`None`: the daemon's
    /// default), when the URL is HTTPS -- git's credential protocol never
    /// fires for SSH, which keeps using the machine's own keys.
    #[must_use]
    pub fn for_clone_url(
        clone_url: &str,
        identity: Option<&str>,
        config_root: &Path,
    ) -> Option<Self> {
        let url = clone_url.trim();
        (url.starts_with("https://") || url.starts_with("http://")).then(|| Self {
            clone_url: url.to_string(),
            identity: identity.map(str::to_string),
            config_root: config_root.to_path_buf(),
            resolved: std::sync::OnceLock::new(),
        })
    }

    fn resolve(&self, store: &crate::store_lock::StoreHandle) -> Option<&GitCredential> {
        self.resolved
            .get_or_init(|| {
                let (host, _) = crate::forge::parse_remote_url(&self.clone_url)?;
                let cfg = crate::config::resolve_forge(&self.config_root);
                let client = crate::forge::resolve_url_with_store(
                    store,
                    &self.clone_url,
                    &cfg,
                    self.identity.as_deref(),
                )
                .ok()?;
                let token = client.token_for_remote_git()?.to_string();
                crate::redact::register(&token);
                let username = match client.kind() {
                    crate::forge::ForgeKind::GitHub => "x-access-token",
                    crate::forge::ForgeKind::GitLab => "oauth2",
                };
                Some(GitCredential {
                    host,
                    username,
                    token,
                })
            })
            .as_ref()
    }

    /// Credentials already resolved to `token` for `host`, for tests that do
    /// not stand up a forge.
    #[cfg(test)]
    fn resolved_for_test(host: &str, token: &str) -> Self {
        let resolved = std::sync::OnceLock::new();
        let _ = resolved.set(Some(GitCredential {
            host: host.to_string(),
            username: "x-access-token",
            token: token.to_string(),
        }));
        Self {
            clone_url: format!("https://{host}/o/r.git"),
            identity: None,
            config_root: PathBuf::new(),
            resolved,
        }
    }

    /// The environment that lets one git command authenticate.
    pub(crate) fn env(&self, store: &crate::store_lock::StoreHandle) -> Vec<(String, String)> {
        let Some(credential) = self.resolve(store) else {
            return Vec::new();
        };
        [
            // An empty helper first resets any the machine configures, so
            // this one is asked.
            ("GIT_CONFIG_COUNT", "2"),
            ("GIT_CONFIG_KEY_0", "credential.helper"),
            ("GIT_CONFIG_VALUE_0", ""),
            ("GIT_CONFIG_KEY_1", "credential.helper"),
            ("GIT_CONFIG_VALUE_1", INLINE_CREDENTIAL_HELPER),
            ("RALPHUS_GIT_HOST", credential.host.as_str()),
            ("RALPHUS_GIT_USERNAME", credential.username),
            ("RALPHUS_GIT_PASSWORD", credential.token.as_str()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }
}

/// One remote-produced branch of a review: the daemon-local project root it
/// belongs to, the machine it ran on, and that cell's workspace on the machine.
struct RemoteBranchSource {
    local_root: PathBuf,
    machine: String,
    remote_cwd: String,
}

/// `remote` joined with the daemon-relative `suffix`: with `/` when `remote`
/// is a POSIX path (whatever this host's separator is), else as this host
/// joins paths.
fn join_on_machine(remote: &Path, suffix: &Path) -> PathBuf {
    let base = remote.to_string_lossy();
    if !base.starts_with('/') {
        return remote.join(suffix);
    }
    let mut out = base.trim_end_matches('/').to_string();
    for part in suffix.components() {
        out.push('/');
        out.push_str(&part.as_os_str().to_string_lossy());
    }
    PathBuf::from(out)
}

/// Every branch of `guardian` produced by a cell on some machine, with that
/// cell's provisioned workspace path. Store reads only.
fn remote_branch_sources(
    conn: &rusqlite::Connection,
    guardian: &crate::guardian::GuardianView,
) -> Vec<RemoteBranchSource> {
    guardian
        .branches
        .iter()
        .filter_map(|b| {
            let machine = b
                .source_cell_machine
                .as_deref()
                .map(str::trim)
                .filter(|m| {
                    !m.is_empty() && !m.eq_ignore_ascii_case(ralphus_core::schema::LOCAL_MACHINE)
                })?;
            let remote_cwd = crate::store::Store::cell_cwd_for_branch_conn(conn, &b.branch)
                .ok()
                .flatten()?;
            Some(RemoteBranchSource {
                local_root: PathBuf::from(b.project.as_deref().unwrap_or(&guardian.git_root)),
                machine: machine.to_string(),
                remote_cwd,
            })
        })
        .collect()
}

/// Each review's `local project root -> repository on its machine` pairs,
/// discovered once per review (the repository a machine provisioned never
/// moves). Lives here rather than on `Store`: it is a cache of a provider's
/// answer, not database state.
static MACHINE_REPOSITORIES: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<(String, PathBuf), PathBuf>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// The `(daemon-local project root, repository on the machine)` pair for the
/// project `root` belongs to, when one of the review's branches was produced on
/// `machine`.
///
/// A review records its project by the daemon's registered path, which is its
/// identity everywhere else (the board, PR routing, reviews keyed by root). On
/// a remote machine that path does not exist: the project lives wherever the
/// provider provisioned it. The repository is discovered from a contributing
/// cell's own workspace there (`git rev-parse --git-common-dir`), so no
/// provider has to report its storage layout.
///
/// When no contributing cell ran on `machine` (e.g. a remote review fed only
/// by local cells), the project is provisioned there for the review itself
/// ([`crate::worktrees::provision_review_repository`]). `None` only when that
/// fails too, which leaves the workspace's paths untouched.
fn machine_repository_for(
    guardian_id: &str,
    machine: &str,
    root: &Path,
    sources: &[RemoteBranchSource],
    review: &ReviewProjects,
    store: &crate::store_lock::StoreHandle,
) -> Option<(PathBuf, PathBuf)> {
    for source in sources.iter().filter(|s| s.machine == machine) {
        if !root.starts_with(&source.local_root) {
            continue;
        }
        let key = (guardian_id.to_string(), source.local_root.clone());
        if let Some(repository) = MACHINE_REPOSITORIES.lock().get(&key).cloned() {
            return Some((source.local_root.clone(), repository));
        }
        let cell_ws =
            Workspace::on(source.remote_cwd.as_str(), Some(machine)).with_store(Arc::clone(store));
        let Ok(common) = cell_ws.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        else {
            continue;
        };
        let Some(repository) = Path::new(common.trim()).parent().map(Path::to_path_buf) else {
            continue;
        };
        MACHINE_REPOSITORIES.lock().insert(key, repository.clone());
        return Some((source.local_root.clone(), repository));
    }
    let local_root = review.local_root_for(root)?;
    let key = (guardian_id.to_string(), local_root.clone());
    if let Some(repository) = MACHINE_REPOSITORIES.lock().get(&key).cloned() {
        return Some((local_root, repository));
    }
    match crate::worktrees::provision_review_repository(
        store,
        guardian_id,
        machine,
        &local_root.to_string_lossy(),
        &review.base_branch,
    ) {
        Ok(repository) => {
            let repository = PathBuf::from(repository);
            MACHINE_REPOSITORIES.lock().insert(key, repository.clone());
            Some((local_root, repository))
        }
        Err(e) => {
            // ralphus[ignore-rlog-pair]: resolved while building a workspace, whose callers may hold the store lock
            crate::rlog!(
                WARNING,
                "ralphus [workspace] review {guardian_id} could not be provisioned on machine \
                 {machine}: {e}"
            );
            None
        }
    }
}

/// The project roots a review spans and the base branch it builds on, for
/// provisioning it on a machine none of its cells ran on.
#[derive(Default)]
struct ReviewProjects {
    git_root: String,
    projects: Vec<String>,
    base_branch: String,
}

impl ReviewProjects {
    /// The review project `root` belongs to: the longest project root it is
    /// under, or the review's own `git_root`.
    fn local_root_for(&self, root: &Path) -> Option<PathBuf> {
        self.projects
            .iter()
            .map(PathBuf::from)
            .filter(|p| root.starts_with(p))
            .max_by_key(|p| p.as_os_str().len())
            .or_else(|| {
                let git_root = PathBuf::from(&self.git_root);
                (!self.git_root.is_empty() && root.starts_with(&git_root)).then_some(git_root)
            })
    }
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.root)
            .field("machine", &self.machine)
            .finish()
    }
}

impl Workspace {
    /// A workspace on the daemon's own host.
    ///
    /// The overwhelmingly common case, and the one that must stay free of
    /// overhead — see the module doc.
    #[must_use]
    pub fn local(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            local_root: root.clone(),
            root,
            machine: None,
            store: None,
            path_map: None,
            forge_credentials: None,
        }
    }

    /// A workspace on `machine`, or a local one when `machine` is unset, empty,
    /// or the reserved `local` value.
    #[must_use]
    pub fn on(root: impl Into<PathBuf>, machine: Option<&str>) -> Self {
        let machine = machine
            .map(str::trim)
            .filter(|m| {
                !m.is_empty() && !m.eq_ignore_ascii_case(ralphus_core::schema::LOCAL_MACHINE)
            })
            .map(str::to_string);
        let root = root.into();
        Self {
            local_root: root.clone(),
            root,
            machine,
            store: None,
            path_map: None,
            forge_credentials: None,
        }
    }

    /// Give this workspace's network git commands a forge credential (see
    /// [`ForgeCredentials`]). Only a remote workspace uses it: a local one
    /// keeps the daemon's own credential helper wiring.
    #[must_use]
    pub fn with_forge_credentials(mut self, credentials: Option<Arc<ForgeCredentials>>) -> Self {
        self.forge_credentials = credentials;
        self
    }

    /// The workspace a guardian's review work happens in, honouring the
    /// machine that review was assigned.
    ///
    /// Resolved **once** per merge rather than per command: the assignment
    /// cannot change mid-merge, and re-reading it for each of the ~70 git
    /// operations a merge performs would take the store lock that many times
    /// for an answer that never moves.
    #[must_use]
    pub fn for_guardian(
        store: &crate::store_lock::StoreHandle,
        guardian_id: &str,
        root: impl Into<PathBuf>,
    ) -> Self {
        let root = root.into();
        // Read through the pool: hydrating the review reads its project config
        // from disk, which held the store lock for seconds under the writer.
        let loaded = store.with_read_snapshot_or_lock_when_shared_cache(|conn| {
            let g = crate::store::Store::get_guardian_conn(conn, guardian_id)?;
            let sources = remote_branch_sources(conn, &g);
            let review = ReviewProjects {
                git_root: g.git_root,
                projects: g.projects,
                base_branch: g.base_branch,
            };
            Ok((g.machine, sources, review, g.owner))
        });
        let (machine, sources, review, owner) = {
            match loaded {
                Ok(found) => found,
                Err(e) => {
                    let guard = store.lock();
                    // A deleted guardian (its retained worktrees still being
                    // retired) is expected; anything else is a store fault.
                    let level = if matches!(e, crate::store::StoreError::NotFound) {
                        crate::logging::LogLevel::DEBUG
                    } else {
                        crate::logging::LogLevel::WARNING
                    };
                    crate::cartographer::Note::new("workspace")
                        .level(level)
                        .scope("guardian")
                        .guardian(guardian_id)
                        .emit(
                            &guard,
                            format!(
                                "could not load guardian {guardian_id} to resolve its machine; \
                                 treating its workspace as local: {e}"
                            ),
                            serde_json::json!({ "error": e.to_string() }),
                        );
                    (None, Vec::new(), ReviewProjects::default(), None)
                }
            }
        };
        // A review that asked a provider service for a machine is granted one
        // the first time it is used (and again after that allocation is
        // released, e.g. by a reopened review).
        let needs_grant = machine
            .as_deref()
            .and_then(|m| crate::allocations::request_needing_grant(store, m));
        let machine = match needs_grant {
            Some(request) => match crate::allocations::bind_review(store, guardian_id, &request) {
                Ok(granted) => Some(granted),
                Err(e) => {
                    crate::cartographer::Note::new("workspace")
                        .level(crate::logging::LogLevel::WARNING)
                        .scope("guardian")
                        .guardian(guardian_id)
                        .emit(
                            &store.lock(),
                            format!("review {guardian_id} has no machine yet: {e}"),
                            serde_json::json!({ "error": e }),
                        );
                    machine
                }
            },
            None => machine,
        };
        let mut workspace = Self::on(root, machine.as_deref()).with_store(Arc::clone(store));
        if let Some(machine) = workspace.machine.clone() {
            workspace.path_map = machine_repository_for(
                guardian_id,
                &machine,
                &workspace.root,
                &sources,
                &review,
                store,
            )
            .map(Arc::new);
            workspace.root = workspace.map_path(&workspace.root);
            // Pushes and fetches on the machine authenticate as the review's
            // owner when the project is cloned over HTTPS.
            let clone_url = {
                let guard = store.lock();
                guard
                    .project_name_for_path(&review.git_root)
                    .and_then(|name| guard.get_project(&name).ok().flatten())
                    .and_then(|project| project.clone_url)
            };
            let identity = crate::forge::effective_forge_identity(None, owner.as_deref());
            workspace.forge_credentials = clone_url
                .and_then(|url| {
                    ForgeCredentials::for_clone_url(
                        &url,
                        identity.as_deref(),
                        &workspace.local_root,
                    )
                })
                .map(Arc::new);
        }
        workspace
    }

    /// Attach the store handle a remote workspace needs to resolve its
    /// provider. A no-op in effect for a local workspace, which never looks.
    #[must_use]
    pub fn with_store(mut self, store: crate::store_lock::StoreHandle) -> Self {
        self.store = Some(store);
        self
    }

    /// This workspace's directory, as named on its own machine.
    ///
    /// Safe to use for path arithmetic (joining a subdirectory, deriving a
    /// sibling). **Not** safe to hand to anything that touches this host's
    /// filesystem — `std::fs`, `Command::current_dir` — when
    /// [`Self::is_local`] is false: the path names a directory over there.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The machine this workspace lives on, or `None` for the daemon's host.
    #[must_use]
    pub fn machine(&self) -> Option<&str> {
        self.machine.as_deref()
    }

    /// Whether this workspace is on the daemon's own host.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.machine.is_none()
    }

    /// A workspace at `path`, on the same machine as this one.
    ///
    /// Used wherever the old code did `root.join(..)` and then ran git there —
    /// the derived directory is on the same host by construction.
    #[must_use]
    pub fn at(&self, path: impl Into<PathBuf>) -> Self {
        let local_root = path.into();
        Self {
            root: self.map_path(&local_root),
            machine: self.machine.clone(),
            store: self.store.clone(),
            path_map: self.path_map.clone(),
            local_root,
            forge_credentials: self.forge_credentials.clone(),
        }
    }

    /// The path the daemon knows this workspace by -- the registered project
    /// path config and project lookups are keyed by -- as opposed to
    /// [`Self::root`], where it is on its machine.
    #[must_use]
    pub fn local_root(&self) -> &Path {
        &self.local_root
    }

    /// Translate a daemon-local path under this review's project root to the
    /// same path on the machine (see `path_map`); anything else is unchanged.
    /// The suffix is joined with `/` when the machine's path is a POSIX one,
    /// so a Windows daemon never writes `\` into a Linux machine's path.
    fn map_path(&self, path: &Path) -> PathBuf {
        match self.path_map.as_deref() {
            Some((local, remote)) => match path.strip_prefix(local) {
                Ok(suffix) if suffix.as_os_str().is_empty() => remote.clone(),
                Ok(suffix) => join_on_machine(remote, suffix),
                Err(_) => path.to_path_buf(),
            },
            None => path.to_path_buf(),
        }
    }

    /// A workspace at `root` joined with `rel`, on the same machine.
    #[must_use]
    pub fn join(&self, rel: impl AsRef<Path>) -> Self {
        self.at(self.root.join(rel))
    }

    /// Run `git` with `args` in this workspace.
    ///
    /// Local workspaces call [`crate::guardian_merge::git`] directly — the same
    /// code path as before this module existed. Remote workspaces route the
    /// command to the machine's provider.
    ///
    /// # Errors
    /// The command's own failure message, or a description of why it could not
    /// be dispatched.
    pub fn git(&self, args: &[&str]) -> Result<String, String> {
        match &self.machine {
            None => crate::guardian_merge::git(&self.root, args),
            Some(machine) => self.git_remote(machine, args),
        }
    }

    /// Run a shell command in this workspace, returning whether it succeeded
    /// and its combined output.
    ///
    /// Used for a review's check gates. Local workspaces go through
    /// [`crate::proof::run_command_proof_capture`] exactly as before; remote
    /// ones send the command to the provider.
    ///
    /// Distinct from [`Self::git`] because a check gate is an arbitrary shell
    /// command (`cargo test`, `npm run build`), not a VCS operation — the
    /// provider may well want to run the two very differently.
    #[must_use]
    pub fn run_command(
        &self,
        command: &str,
        cancel: &crate::cancel::CancelToken,
    ) -> (bool, String) {
        self.run_command_with_env(command, &std::collections::BTreeMap::new(), cancel)
    }

    /// Like [`Self::run_command`] but applies `env` on top of the daemon's own
    /// inherited environment (RAL-191) — used for a review's check gates, so
    /// they run under the same variables as the branch's agent invocations.
    ///
    /// A remote workspace sends `env` in the
    /// [`crate::remote_runner::RunRequest`] for the provider to apply on the
    /// machine. `cancel` is local-only (RAL-239): a remote provider call has
    /// no polling hook to kill mid-flight, so it blocks to completion.
    #[must_use]
    pub fn run_command_with_env(
        &self,
        command: &str,
        env: &std::collections::BTreeMap<String, String>,
        cancel: &crate::cancel::CancelToken,
    ) -> (bool, String) {
        match &self.machine {
            None => crate::proof::run_command_proof_capture(
                &self.root.to_string_lossy(),
                command,
                &opentelemetry::Context::new(),
                env,
                cancel,
            ),
            Some(_) => {
                // Split on whitespace is wrong for a shell command, so the
                // provider is handed the whole string and runs it through a
                // shell on its own side -- the one place the args-already-split
                // rule cannot apply, because the author wrote shell syntax.
                let req = crate::remote_runner::RunRequest {
                    cwd: remote_path_string(&self.root),
                    program: String::new(),
                    args: vec![command.to_string()],
                    env: env.clone(),
                    shell: crate::config::resolve_command_shell(&self.root),
                };
                match self.with_provider(|p, spec| p.run_vcs(&req, spec)) {
                    Ok(out) => (true, out),
                    Err(e) => (false, e),
                }
            }
        }
    }

    /// Run a local command with both review cancellation and a bounded wall
    /// clock duration. Remote providers do not yet expose a cancellation-aware
    /// timeout contract, so V1 rejects that combination at the caller.
    #[must_use]
    pub fn run_command_with_env_timeout(
        &self,
        command: &str,
        env: &std::collections::BTreeMap<String, String>,
        cancel: &crate::cancel::CancelToken,
        timeout: std::time::Duration,
    ) -> (bool, String) {
        match &self.machine {
            None => crate::proof::run_command_proof_capture_with_timeout(
                &self.root.to_string_lossy(),
                command,
                &opentelemetry::Context::new(),
                env,
                cancel,
                Some(timeout),
            ),
            Some(_) => (
                false,
                "lifecycle timeout is not supported by a remote provider yet".to_string(),
            ),
        }
    }

    /// Read a file at `path`, which may be absolute or relative to this
    /// workspace's root.
    ///
    /// `None` when it does not exist or cannot be read — every caller in the
    /// merge path treats "absent" and "unreadable" the same way, so collapsing
    /// them keeps those call sites honest instead of inventing a distinction
    /// nobody acts on.
    #[must_use]
    pub fn read_file(&self, path: impl AsRef<Path>) -> Option<String> {
        let full = self.resolve(path);
        match &self.machine {
            None => std::fs::read_to_string(&full).ok(),
            Some(_) => self
                .with_provider(|p, spec| p.read_file(&remote_path_string(&full), spec))
                .ok(),
        }
    }

    /// Write `content` to `path`, creating parent directories as needed.
    ///
    /// # Errors
    /// Any failure to write, with the path included — a merge that could not
    /// lay down a worktree link file needs to say which one.
    pub fn write_file(&self, path: impl AsRef<Path>, content: &str) -> Result<(), String> {
        let full = self.resolve(path);
        match &self.machine {
            None => {
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
                }
                std::fs::write(&full, content)
                    .map_err(|e| format!("could not write {}: {e}", full.display()))
            }
            Some(_) => self
                .with_provider(|p, spec| p.write_file(&remote_path_string(&full), content, spec)),
        }
    }

    /// Write an executable file at `path`, creating parent directories as
    /// needed. This is used for Git hooks, which Git ignores on Unix unless
    /// their executable bit is set.
    pub fn write_executable_file(
        &self,
        path: impl AsRef<Path>,
        content: &str,
    ) -> Result<(), String> {
        let full = self.resolve(path);
        match &self.machine {
            None => {
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
                }
                std::fs::write(&full, content)
                    .map_err(|e| format!("could not write {}: {e}", full.display()))?;
                set_executable(&full)
            }
            Some(_) => self.with_provider(|p, spec| {
                p.write_executable_file(&remote_path_string(&full), content, spec)
            }),
        }
    }

    /// Delete a file, or a directory tree when `recursive`.
    ///
    /// Best-effort by design: a path that does not exist is success, because
    /// every caller here is cleaning up and does not care.
    pub fn remove_path(&self, path: impl AsRef<Path>, recursive: bool) {
        let full = self.resolve(path);
        match &self.machine {
            None => {
                let removed = if recursive {
                    std::fs::remove_dir_all(&full)
                } else {
                    std::fs::remove_file(&full)
                };
                match removed {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        // ralphus[ignore-rlog-pair]: callers may already hold the store lock, so locking it here to emit could deadlock
                        crate::rlog!(
                            WARNING,
                            "ralphus [workspace] could not remove {} (recursive={recursive}): {e}",
                            full.display()
                        );
                    }
                }
            }
            Some(machine) => {
                if let Err(e) = self.with_provider(|p, spec| {
                    p.remove_path(&remote_path_string(&full), recursive, spec)
                }) {
                    // ralphus[ignore-rlog-pair]: callers may already hold the store lock, so locking it here to emit could deadlock
                    crate::rlog!(
                        WARNING,
                        "ralphus [workspace] could not remove {} on machine {machine} \
                         (recursive={recursive}): {e}",
                        full.display()
                    );
                }
            }
        }
    }

    /// Whether `path` exists.
    ///
    /// Implemented as a read on a remote workspace, so it costs a round trip —
    /// prefer restructuring a hot loop over calling this repeatedly.
    #[must_use]
    pub fn exists(&self, path: impl AsRef<Path>) -> bool {
        match &self.machine {
            None => self.resolve(path).exists(),
            Some(_) => self.read_file(path).is_some(),
        }
    }

    /// Whether this workspace's directory exists on its machine: the directory
    /// itself for a local workspace; the `.git` entry a worktree keeps for a
    /// remote one, since a provider can read a file but has no verb that
    /// tests for a directory. Meant for worktree directories.
    #[must_use]
    pub fn worktree_present(&self) -> bool {
        if self.is_local() {
            self.root.exists()
        } else {
            self.exists(".git")
        }
    }

    /// Copy one selected file or directory from this workspace into an exact
    /// absolute destination on the daemon host.
    pub fn materialize_to_daemon(
        &self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        executable: bool,
    ) -> Result<(), String> {
        let source = self.resolve(source);
        let destination = destination.as_ref();
        if !destination.is_absolute() {
            return Err(format!(
                "artifact destination must be absolute on the daemon host: {}",
                destination.display()
            ));
        }
        match &self.machine {
            None => {
                let allowed_root = self
                    .root
                    .canonicalize()
                    .map_err(|e| format!("could not resolve workspace root: {e}"))?;
                let source_resolved = source
                    .canonicalize()
                    .map_err(|e| format!("could not resolve artifact {}: {e}", source.display()))?;
                if destination.starts_with(&source_resolved) {
                    return Err("artifact destination cannot be inside its source".to_string());
                }
                let parent = destination.parent().ok_or_else(|| {
                    format!(
                        "artifact destination has no parent: {}",
                        destination.display()
                    )
                })?;
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                let mut nonce = [0_u8; 8];
                getrandom::getrandom(&mut nonce)
                    .map_err(|e| format!("could not create artifact transfer id: {e}"))?;
                let staging = parent.join(format!(
                    ".ralphus-artifact-{}",
                    nonce
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                ));
                let publish = (|| {
                    copy_local_artifact(&source, &staging, executable, &allowed_root)?;
                    remove_local_destination(destination)?;
                    std::fs::rename(&staging, destination).map_err(|e| {
                        format!("could not publish artifact {}: {e}", destination.display())
                    })
                })();
                if publish.is_err() {
                    let _ = remove_local_destination(&staging);
                }
                publish
            }
            Some(_) => self.with_provider(|provider, spec| {
                provider.materialize(
                    &remote_path_string(&source),
                    &destination.to_string_lossy(),
                    executable,
                    spec,
                )
            }),
        }
    }

    /// Platform reported by the machine that owns this workspace.
    pub fn target_platform(&self) -> Result<(Option<String>, Option<String>), String> {
        if self.machine.is_none() {
            return Ok((
                Some(std::env::consts::OS.to_string()),
                Some(std::env::consts::ARCH.to_string()),
            ));
        }
        self.with_provider(|provider, spec| {
            let capabilities = provider
                .capabilities(spec)?
                .ok_or_else(|| "machine provider did not report a target platform".to_string())?;
            Ok((capabilities.os, capabilities.arch))
        })
    }

    /// Resolve `path` against this workspace's root when relative, or take it
    /// as-is when already absolute.
    ///
    /// "Absolute" follows the machine, not the daemon: on a remote workspace
    /// `/srv/repo` is absolute even when the daemon runs on Windows, where
    /// [`Path::is_absolute`] would call it relative and join it onto the root.
    pub fn resolve(&self, path: impl AsRef<Path>) -> PathBuf {
        let p = path.as_ref();
        let absolute = if self.machine.is_some() {
            is_absolute_anywhere(p)
        } else {
            p.is_absolute()
        };
        if absolute {
            self.map_path(p)
        } else {
            self.root.join(p)
        }
    }

    /// This workspace's root as a string that names it on its own machine:
    /// the native form for a local workspace, `/`-separated for a remote one
    /// (see [`remote_path_string`]). Use it wherever the root is stored or
    /// handed on as text.
    #[must_use]
    pub fn root_string(&self) -> String {
        if self.machine.is_some() {
            remote_path_string(&self.root)
        } else {
            self.root.to_string_lossy().into_owned()
        }
    }

    /// Run `f` against this workspace's provider.
    ///
    /// Every remote operation needs the same three steps — find the store,
    /// resolve the provider, build a throwaway spec — so they live here once.
    fn with_provider<T>(
        &self,
        f: impl FnOnce(
            &crate::remote_runner::ProviderRunner,
            &crate::runner::RunnerSpec,
        ) -> Result<T, String>,
    ) -> Result<T, String> {
        let machine = self
            .machine
            .as_deref()
            .ok_or_else(|| "not a remote workspace".to_string())?;
        let Some(store) = &self.store else {
            return Err(format!(
                "cannot reach machine \"{machine}\": this workspace was built without a store                  handle, so its provider cannot be resolved. This is a bug -- use                  `Workspace::for_guardian`, which supplies one."
            ));
        };
        let provider = {
            let guard = store.lock();
            crate::remote_runner::provider_from_store(&guard, machine)?.ok_or_else(|| {
                format!("machine \"{machine}\" resolved to the local host unexpectedly")
            })?
        };
        // These operations belong to no cell; the spec exists only to
        // satisfy the invocation shape.
        let spec = crate::runner::RunnerSpec::for_command_proof(
            "review-merge",
            "review-merge",
            machine,
            ".",
            "",
            "claude",
            None,
        );
        f(&provider, &spec)
    }

    /// Retire the worktree at `path` in this workspace (RAL-386).
    ///
    /// The abstraction the daily worktree-retirement sweep
    /// (`guardian_merge::retire_stale_worktrees`) uses instead of calling
    /// [`Self::git`] directly, so local and remote retirement share one seam
    /// the way every other operation on this type already does. A local
    /// workspace runs exactly the `git worktree remove` it always did --
    /// unaffected by, and unaware of, anything RAL-386 added. A remote one
    /// asks the machine's provider via its `retire` verb, whose default for
    /// a provider that has never heard of it is a safe
    /// [`RetirementOutcome::OptedOut`], never a silent deletion attempt
    /// against an unrelated verb.
    #[must_use]
    pub fn retire_worktree(&self, path: &str) -> crate::remote_runner::RetirementOutcome {
        use crate::remote_runner::RetirementOutcome;
        match &self.machine {
            None => match crate::guardian_merge::git(
                &self.root,
                &["worktree", "remove", "--force", "--force", path],
            ) {
                Ok(_) => RetirementOutcome::Removed,
                Err(error) => RetirementOutcome::Failed { error },
            },
            Some(_) => {
                let req = crate::remote_runner::RetireRequest {
                    path: path.to_string(),
                };
                match self.with_provider(|p, spec| p.retire(&req, spec)) {
                    Ok(outcome) => outcome,
                    Err(error) => RetirementOutcome::Failed { error },
                }
            }
        }
    }

    /// Dispatch a git command to a remote machine's provider.
    ///
    /// Deliberately kept out of [`Self::git`]'s hot path so the local case is a
    /// single match arm and a direct call.
    ///
    /// One provider invocation per command. Whether that is a fresh SSH
    /// connection, a multiplexed one, or a message into a persistent worker is
    /// the provider's business — the contract says nothing about transport, so
    /// a farm on a LAN and a machine across a slow link can make different
    /// choices without ralphus changing (RAL-185 D7).
    ///
    /// Carries the same `GIT_EDITOR`/`GIT_SEQUENCE_EDITOR=true` the local path
    /// sets (`crate::vcs::GitVcs`): without them `rebase --continue` on the
    /// machine opens an editor with no terminal, fails, and the merge retries
    /// it forever.
    fn git_remote(&self, _machine: &str, args: &[&str]) -> Result<String, String> {
        let mut env = non_interactive_git_env();
        let needs_credential = args
            .first()
            .is_some_and(|verb| NETWORK_GIT_COMMANDS.contains(verb));
        if let (true, Some(credentials), Some(store)) =
            (needs_credential, &self.forge_credentials, &self.store)
        {
            env.extend(credentials.env(store));
        }
        let req = crate::remote_runner::RunRequest {
            cwd: remote_path_string(&self.root),
            program: "git".to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            env,
            shell: None,
        };
        self.with_provider(|p, spec| p.run_vcs(&req, spec))
    }
}

/// `path` as text for a remote machine, with every separator a `/`.
///
/// A remote path is built with [`Path::join`], which uses the *daemon's*
/// separator: a Windows daemon joining onto a Linux machine's `/srv/repo`
/// yields `/srv/repo\.git\worktrees`, which names nothing over there. `/` is
/// read correctly by POSIX machines and by Windows ones (git and the Win32
/// file APIs both accept it), so normalising to it works for either. A
/// backslash inside a POSIX file name is the one casualty, and ralphus never
/// creates such a name.
fn remote_path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Whether `path` is absolute on POSIX (`/x`) or Windows (`C:\x`, `C:/x`,
/// `\\server\x`), whatever OS the daemon itself runs on.
fn is_absolute_anywhere(path: &Path) -> bool {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    path.is_absolute()
        || bytes.first().is_some_and(|b| *b == b'/' || *b == b'\\')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'/' || bytes[2] == b'\\'))
}

/// Somewhere `git` can run: a directory on the daemon's host, or a
/// [`Workspace`] on whatever machine it names.
///
/// Lets a helper that only *runs git* (resolving a remote name, reading a
/// remote's URL) take either, so local callers keep passing a path while a
/// remote review's callers pass its workspace and the commands run there.
pub trait GitAt {
    /// Run `git` with `args` here, returning stdout.
    ///
    /// # Errors
    /// The command's failure message.
    fn git_at(&self, args: &[&str]) -> Result<String, String>;

    /// The path the daemon knows this place by, for config, project and
    /// cache lookups keyed by the registered project path -- never for
    /// running anything.
    fn local_path(&self) -> &Path;
}

impl GitAt for Path {
    fn git_at(&self, args: &[&str]) -> Result<String, String> {
        crate::guardian_merge::git(self, args)
    }

    fn local_path(&self) -> &Path {
        self
    }
}

impl GitAt for PathBuf {
    fn git_at(&self, args: &[&str]) -> Result<String, String> {
        crate::guardian_merge::git(self, args)
    }

    fn local_path(&self) -> &Path {
        self
    }
}

impl GitAt for Workspace {
    fn git_at(&self, args: &[&str]) -> Result<String, String> {
        self.git(args)
    }

    fn local_path(&self) -> &Path {
        self.local_root()
    }
}

/// The environment every remote `git` invocation runs under, so nothing it
/// does can wait on an interactive editor.
fn non_interactive_git_env() -> std::collections::BTreeMap<String, String> {
    ["GIT_EDITOR", "GIT_SEQUENCE_EDITOR"]
        .into_iter()
        .map(|k| (k.to_string(), "true".to_string()))
        .collect()
}

fn remove_local_destination(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path).map_err(|e| e.to_string())
        }
        Ok(_) => std::fs::remove_file(path).map_err(|e| e.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn copy_local_artifact(
    source: &Path,
    destination: &Path,
    executable: bool,
    allowed_root: &Path,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|e| format!("could not inspect artifact {}: {e}", source.display()))?;
    if metadata.file_type().is_symlink() {
        let resolved = source.canonicalize().map_err(|e| {
            format!(
                "could not resolve artifact symlink {}: {e}",
                source.display()
            )
        })?;
        if !resolved.starts_with(allowed_root) {
            return Err(format!(
                "artifact symlink escapes its declared root: {}",
                source.display()
            ));
        }
        return copy_local_artifact(&resolved, destination, executable, allowed_root);
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "could not create artifact destination {}: {e}",
                parent.display()
            )
        })?;
    }
    if metadata.is_dir() {
        std::fs::create_dir_all(destination).map_err(|e| {
            format!(
                "could not create artifact directory {}: {e}",
                destination.display()
            )
        })?;
        for entry in std::fs::read_dir(source).map_err(|e| {
            format!(
                "could not read artifact directory {}: {e}",
                source.display()
            )
        })? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_local_artifact(
                &entry.path(),
                &destination.join(entry.file_name()),
                executable,
                allowed_root,
            )?;
        }
    } else {
        std::fs::copy(source, destination).map_err(|e| {
            format!(
                "could not copy artifact {} to {}: {e}",
                source.display(),
                destination.display()
            )
        })?;
        if executable {
            set_executable(destination)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)
        .map_err(|e| format!("could not read permissions for {}: {e}", path.display()))?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(path, permissions).map_err(|e| {
        format!(
            "could not set executable permissions on {}: {e}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
pub(crate) fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_posix_machine_path_is_joined_with_slashes() {
        let suffix = std::path::Path::new(".git")
            .join(".ralphus")
            .join("g")
            .join("g5");
        assert_eq!(
            super::join_on_machine(std::path::Path::new("/srv/repo"), &suffix),
            std::path::PathBuf::from("/srv/repo/.git/.ralphus/g/g5")
        );
        assert_eq!(
            super::join_on_machine(
                std::path::Path::new("/srv/repo/"),
                std::path::Path::new("w")
            ),
            std::path::PathBuf::from("/srv/repo/w")
        );
    }

    use super::*;

    #[test]
    fn local_artifact_materialization_publishes_an_exact_directory() {
        let root = ralphus_test_support::scratch(format!(
            "ralphus-artifact-copy-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let source = root.join("out");
        let destination = root.join("prepared").join("app");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("new.txt"), "new").unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("stale.txt"), "stale").unwrap();

        Workspace::local(&root)
            .materialize_to_daemon("out", &destination, false)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(destination.join("new.txt")).unwrap(),
            "new"
        );
        assert!(!destination.join("stale.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn local_workspace_reports_the_action_platform() {
        assert_eq!(
            Workspace::local(ralphus_test_support::scratch_root())
                .target_platform()
                .unwrap(),
            (
                Some(std::env::consts::OS.to_string()),
                Some(std::env::consts::ARCH.to_string())
            )
        );
    }

    #[test]
    fn an_unset_or_local_machine_yields_a_local_workspace() {
        // Every pre-RAL-185 review, plus one that says `local` explicitly.
        assert!(Workspace::on("/repo", None).is_local());
        assert!(Workspace::on("/repo", Some("")).is_local());
        assert!(Workspace::on("/repo", Some("   ")).is_local());
        assert!(Workspace::on("/repo", Some("local")).is_local());
        assert!(Workspace::on("/repo", Some("LOCAL")).is_local());
    }

    #[test]
    fn a_provider_machine_yields_a_remote_workspace() {
        let ws = Workspace::on("/repo", Some("incredibuild:A"));
        assert!(!ws.is_local());
        assert_eq!(ws.machine(), Some("incredibuild:A"));
    }

    #[test]
    fn derived_workspaces_stay_on_the_same_machine() {
        // The failure this prevents: joining a subdirectory and silently
        // dropping back to the local host, so half a merge runs in the wrong
        // place.
        let ws = Workspace::on("/repo", Some("incredibuild:A"));
        let wt = ws.join("worktrees/feat");
        assert_eq!(wt.machine(), Some("incredibuild:A"));
        assert_eq!(wt.root(), Path::new("/repo/worktrees/feat"));

        let local = Workspace::local("/repo");
        assert!(local.join("sub").is_local());
    }

    #[test]
    fn remote_paths_are_sent_with_forward_slashes() {
        assert_eq!(
            remote_path_string(Path::new(r"/srv/repo\.git\worktrees")),
            "/srv/repo/.git/worktrees"
        );
        assert_eq!(
            remote_path_string(Path::new(r"C:\work\repo")),
            "C:/work/repo"
        );
    }

    #[test]
    fn a_subdirectory_of_a_posix_remote_root_is_named_with_forward_slashes() {
        // On a Windows daemon `Path::join` writes `\`, which a Linux machine
        // reads as part of a file name.
        let ws = Workspace::on("/srv/repo", Some("incredibuild:A")).join(".git/worktrees/feat");
        assert_eq!(ws.root_string(), "/srv/repo/.git/worktrees/feat");
    }

    #[test]
    fn a_mapped_review_path_is_named_with_forward_slashes_on_the_machine() {
        // The review derives `.git/.ralphus/...` from the daemon's own project
        // root and translates it onto the machine's repository; the suffix
        // carries the daemon's separators until it is normalised.
        let local = ralphus_test_support::scratch("ralphus-ws-map");
        let mut ws = Workspace::on(local.clone(), Some("incredibuild:A"));
        ws.path_map = Some(Arc::new((local.clone(), PathBuf::from("/srv/repo"))));
        let derived = ws.at(local.join(".git").join(".ralphus").join("g"));
        assert_eq!(derived.root_string(), "/srv/repo/.git/.ralphus/g");
    }

    #[test]
    fn a_remote_workspace_treats_any_os_absolute_path_as_absolute() {
        // `Path::is_absolute` follows the daemon's OS: on Windows `/srv/x` is
        // relative and would be joined onto the workspace root.
        let ws = Workspace::on("/srv/repo", Some("incredibuild:A"));
        assert_eq!(
            remote_path_string(&ws.resolve("/srv/repo/.git/hooks")),
            "/srv/repo/.git/hooks"
        );
        assert_eq!(
            remote_path_string(&ws.resolve(r"C:\work\hooks")),
            "C:/work/hooks"
        );
        assert_eq!(
            remote_path_string(&ws.resolve(".git/hooks")),
            "/srv/repo/.git/hooks"
        );
    }

    #[test]
    fn absolute_paths_are_recognised_for_every_os() {
        for absolute in ["/srv/x", r"\srv\x", r"C:\x", "c:/x", r"\\server\share"] {
            assert!(is_absolute_anywhere(Path::new(absolute)), "{absolute}");
        }
        for relative in ["x", ".git/hooks", r"x\y", "C:x", ""] {
            assert!(!is_absolute_anywhere(Path::new(relative)), "{relative}");
        }
    }

    #[test]
    fn a_local_workspace_actually_runs_git() {
        // Proves the local path is a real passthrough, not a stub.
        let ws = Workspace::local(ralphus_test_support::scratch_root());
        let out = ws.git(&["--version"]).expect("git --version");
        assert!(out.contains("git version"), "{out}");
    }

    #[test]
    fn a_remote_workspace_without_a_store_says_so_instead_of_running_locally() {
        // The dangerous alternative is falling through to the local host, which
        // would run a review's commands on the wrong machine while looking
        // entirely successful. A workspace built without a store handle cannot
        // resolve its provider -- that is a programming error, and it says so
        // rather than quietly doing the wrong thing.
        let ws = Workspace::on("/repo", Some("incredibuild:A"));
        let err = ws.git(&["status"]).expect_err("must not run locally");
        assert!(err.contains("incredibuild:A"), "{err}");
        assert!(err.contains("without"), "{err}");
    }

    #[test]
    fn a_remote_workspace_with_an_unregistered_machine_fails_rather_than_running_locally() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ws = Workspace::on("/repo", Some("ghostfarm:A")).with_store(store);
        let err = ws.git(&["status"]).expect_err("must not run locally");
        assert!(err.contains("ghostfarm"), "{err}");
    }

    #[test]
    fn a_remote_workspace_dispatches_to_its_providers_run_verb() {
        // End-to-end through a real provider program: proves the remote path
        // actually reaches the machine rather than erroring somewhere earlier.
        let dir = ralphus_test_support::scratch(format!("ral185-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let json = r#"{"ok":true,"protocol_version":1,"exit_code":0,"stdout":"deadbeef"}"#;
        let (script, body) = if cfg!(windows) {
            (
                dir.join("p.cmd"),
                format!(
                    "@echo off

echo {json}

"
                ),
            )
        } else {
            (dir.join("p.sh"), format!("#!/bin/sh\necho '{json}'\n"))
        };
        std::fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/remote/repo", Some("ib:A")).with_store(store);
        let out = ws.git(&["rev-parse", "HEAD"]).expect("remote git");
        assert_eq!(out.trim(), "deadbeef");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_remote_command_reports_the_command_not_just_the_exit_code() {
        // A merge failure has to say which git command failed, or debugging it
        // means guessing.
        let dir = ralphus_test_support::scratch(format!("ral185-wsfail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let json =
            r#"{"ok":true,"protocol_version":1,"exit_code":1,"stdout":"not a git repository"}"#;
        let (script, body) = if cfg!(windows) {
            (
                dir.join("p.cmd"),
                format!(
                    "@echo off

echo {json}

"
                ),
            )
        } else {
            (dir.join("p.sh"), format!("#!/bin/sh\necho '{json}'\n"))
        };
        std::fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/remote/repo", Some("ib:A")).with_store(store);
        let err = ws.git(&["status"]).expect_err("non-zero exit must fail");
        assert!(err.contains("git status"), "must name the command: {err}");
        assert!(err.contains("exit 1"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_initialization_runs_on_a_remote_cells_machine() {
        let dir = ralphus_test_support::scratch(format!("ral-d6-init-{}", std::process::id()));
        let script = write_script(
            &dir,
            r#"{"ok":true,"protocol_version":1,"exit_code":0,"stdout":"installed"}"#,
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/srv/wt", Some("ib:A")).with_store(store);
        let events = Arc::new(parking_lot::Mutex::new(0_usize));
        let seen = Arc::clone(&events);
        let spec = crate::worktree_init::InitSpec {
            commands: vec!["npm ci".to_string()],
            timeout: std::time::Duration::from_secs(60),
            env: std::collections::BTreeMap::new(),
            rerun_existing: true,
            recorder: Arc::new(move |_| *seen.lock() += 1),
        };
        let outcome = crate::worktree_init::run_in(&ws, &spec).expect("init succeeds");
        assert!(outcome.ok);
        assert!(outcome.output.contains("installed"), "{outcome:?}");
        assert_eq!(*events.lock(), 2, "started + finished");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remote_branchs_emptiness_is_judged_on_its_machine() {
        // The daemon has no such repository; only the machine can answer.
        let dir = ralphus_test_support::scratch(format!("ral-b3-empty-{}", std::process::id()));
        let script = write_script(
            &dir,
            r#"{"ok":true,"protocol_version":1,"exit_code":0,"stdout":"src/changed.rs"}"#,
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/srv/repo", Some("ib:A")).with_store(Arc::clone(&store));
        assert_eq!(
            crate::guardian_merge::branch_diff_is_empty(&store, &ws, "main", "feature"),
            Ok(false)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rebase_on_a_remote_machine_is_detected_there() {
        // The state directory exists only on the machine; a local
        // `Path::exists` would always say no rebase is running.
        let dir = ralphus_test_support::scratch(format!("ral-b1-rebase-{}", std::process::id()));
        let script = write_script(
            &dir,
            r#"{"ok":true,"protocol_version":1,"exit_code":0,"stdout":"/srv/repo/.git/worktrees/wt/rebase-merge"}"#,
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/srv/wt", Some("ib:A")).with_store(store);
        assert!(crate::guardian_merge::rebase_in_progress(&ws));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remote_reviews_forge_remote_is_resolved_on_its_machine() {
        // PR routing reads remote names from the review's repository. For a
        // remote review that is the machine's, whose git answers here with an
        // upstream on a remote the daemon's own checkout does not have.
        let dir = ralphus_test_support::scratch(format!("ral-a1-remote-{}", std::process::id()));
        let script = write_script(
            &dir,
            r#"{"ok":true,"protocol_version":1,"exit_code":0,"stdout":"machine-remote/main"}"#,
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/srv/repo", Some("ib:A")).with_store(store);

        let remote = crate::forge::resolve_parent_remote_name(
            &ws,
            "main",
            &crate::config::ForgeConfig::default(),
            None,
            None,
        );

        assert_eq!(remote, "machine-remote");
        assert_eq!(GitAt::local_path(&ws), Path::new("/srv/repo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_inline_credential_helper_answers_real_git_for_its_host_only() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let env = ForgeCredentials::resolved_for_test("github.com", "tok-for-test").env(&store);
        let fill = |host: &str| {
            let mut command = std::process::Command::new("git");
            command
                .args(["credential", "fill"])
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            for (k, v) in &env {
                command.env(k, v);
            }
            let mut child = command.spawn().unwrap();
            std::io::Write::write_all(
                &mut child.stdin.take().unwrap(),
                format!("protocol=https\nhost={host}\n\n").as_bytes(),
            )
            .unwrap();
            String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
        };

        let answered = fill("github.com");
        assert!(answered.contains("username=x-access-token"), "{answered}");
        assert!(answered.contains("password=tok-for-test"), "{answered}");
        let other = fill("evil.example");
        assert!(
            !other.contains("tok-for-test"),
            "another host got the token: {other}"
        );
    }

    #[test]
    fn only_network_git_commands_on_a_machine_carry_the_credential() {
        let dir = std::env::temp_dir().join(format!("ral-cred-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let script = write_script(
            &dir,
            r#"{"ok":true,"protocol_version":1,"exit_code":0,"stdout":""}"#,
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/m/repo", Some("ib:A"))
            .with_store(Arc::clone(&store))
            .with_forge_credentials(Some(Arc::new(ForgeCredentials::resolved_for_test(
                "github.com",
                "tok-for-test",
            ))));

        ws.git(&["status"]).unwrap();
        let after_status = std::fs::read_to_string(dir.join("requests.log")).unwrap();
        ws.git(&["push", "origin", "main"]).unwrap();
        let all = std::fs::read_to_string(dir.join("requests.log")).unwrap();

        assert!(!after_status.contains("tok-for-test"), "{after_status}");
        assert!(all[after_status.len()..].contains("tok-for-test"), "{all}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_review_on_a_machine_none_of_its_cells_used_is_provisioned_there() {
        // With no contributing cell on the review's machine there is no
        // workspace to discover the repository from; the review provisions
        // the project itself instead of sending the daemon's own paths.
        let dir = ralphus_test_support::scratch(format!("ral-a8-ws-{}", std::process::id()));
        // One reply answers both verbs the fallback makes: `provision` reads
        // `workspace`, the follow-up `git rev-parse --git-common-dir` reads
        // `stdout`.
        let script = write_script(
            &dir,
            r#"{"ok":true,"protocol_version":1,"workspace":"/m/root/wt","exit_code":0,"stdout":"/m/root/repo/.git"}"#,
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let local_root = dir.join("project");
        let local_root_str = local_root.to_string_lossy().into_owned();
        let id = {
            let guard = store.lock();
            guard
                .register_machine_provider(
                    "ib",
                    "",
                    &script.to_string_lossy(),
                    &[],
                    crate::machines::PROTOCOL_VERSION,
                    false,
                )
                .unwrap();
            // Not git, so no clone URL or machine target is required -- the
            // fallback itself is what is under test.
            guard
                .register_project("p", "", &local_root_str, "svn")
                .unwrap();
            let id = guard.create_guardian("r", "main", &local_root_str).unwrap();
            guard.set_guardian_machine(&id, Some("ib:A")).unwrap();
            id
        };

        let ws = Workspace::for_guardian(&store, &id, local_root.join(".git").join(".ralphus"));

        assert_eq!(ws.machine(), Some("ib:A"));
        assert_eq!(ws.root_string(), "/m/root/repo/.git/.ralphus");
        // A provider clone has its base branch checked out, which git refuses
        // to fetch into; provisioning detaches it so the base can advance.
        let requests = std::fs::read_to_string(dir.join("requests.log")).unwrap();
        assert!(requests.contains("\"--detach\""), "{requests}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fake provider that answers every verb with `json` and appends each
    /// request payload it receives to `requests.log` beside it.
    fn write_script(dir: &Path, json: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let log = dir.join("requests.log");
        let (script, body) = if cfg!(windows) {
            (
                dir.join("p.cmd"),
                format!(
                    "@echo off\r\nmore >> \"{}\"\r\necho {json}\r\n",
                    log.display()
                ),
            )
        } else {
            (
                dir.join("p.sh"),
                format!("#!/bin/sh\ncat >> '{}'\necho '{json}'\n", log.display()),
            )
        };
        std::fs::write(&script, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    #[test]
    fn retire_worktree_dispatches_to_the_providers_retire_verb() {
        // RAL-386: the abstraction seam a remote worktree's automatic
        // retirement goes through -- proves it actually reaches the
        // machine's provider via `retire` rather than attempting a local
        // `git worktree remove` against a path that isn't on this host.
        let dir = ralphus_test_support::scratch(format!("ral386-ws-retire-{}", std::process::id()));
        let script = write_script(
            &dir,
            r#"{"ok":true,"protocol_version":1,"outcome":"deferred","reason":"still in use","retry_at_ms":99}"#,
        );
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        store
            .lock()
            .register_machine_provider(
                "ib",
                "",
                &script.to_string_lossy(),
                &[],
                crate::machines::PROTOCOL_VERSION,
                false,
            )
            .unwrap();
        let ws = Workspace::on("/remote/repo", Some("ib:A")).with_store(store);
        let outcome = ws.retire_worktree("/remote/repo/worktrees/feat");
        assert_eq!(
            outcome,
            crate::remote_runner::RetirementOutcome::Deferred {
                retry_at_ms: Some(99),
                reason: Some("still in use".to_string()),
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retire_worktree_on_an_unregistered_remote_machine_fails_rather_than_running_locally() {
        let store = Arc::new(crate::store_lock::StoreMutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ws = Workspace::on("/remote/repo", Some("ghostfarm:A")).with_store(store);
        match ws.retire_worktree("/remote/repo/worktrees/feat") {
            crate::remote_runner::RetirementOutcome::Failed { error } => {
                assert!(error.contains("ghostfarm"), "{error}");
            }
            other => panic!("must fail rather than silently succeed or opt out: {other:?}"),
        }
    }
}
