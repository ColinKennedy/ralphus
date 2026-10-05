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
}

/// One remote-produced branch of a review: the daemon-local project root it
/// belongs to, the machine it ran on, and that cell's workspace on the machine.
struct RemoteBranchSource {
    local_root: PathBuf,
    machine: String,
    remote_cwd: String,
}

/// Every branch of `guardian` produced by a cell on some machine, with that
/// cell's provisioned workspace path. Store reads only.
fn remote_branch_sources(
    store: &crate::store::Store,
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
            let remote_cwd = store.cell_cwd_for_branch(&b.branch).ok().flatten()?;
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
/// `None` when no contributing cell ran on `machine` (e.g. a remote review fed
/// only by local cells), which leaves the workspace's paths untouched.
fn machine_repository_for(
    guardian_id: &str,
    machine: &str,
    root: &Path,
    sources: &[RemoteBranchSource],
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
    None
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
        Self {
            root: root.into(),
            machine: None,
            store: None,
            path_map: None,
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
        Self {
            root: root.into(),
            machine,
            store: None,
            path_map: None,
        }
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
        let (machine, sources) = {
            let guard = store.lock();
            match guard.get_guardian(guardian_id) {
                Ok(g) => {
                    let sources = remote_branch_sources(&guard, &g);
                    (g.machine, sources)
                }
                Err(e) => {
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
                    (None, Vec::new())
                }
            }
        };
        let mut workspace = Self::on(root, machine.as_deref()).with_store(Arc::clone(store));
        if let Some(machine) = workspace.machine.clone() {
            workspace.path_map =
                machine_repository_for(guardian_id, &machine, &workspace.root, &sources, store)
                    .map(Arc::new);
            workspace.root = workspace.map_path(&workspace.root);
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
        Self {
            root: self.map_path(&path.into()),
            machine: self.machine.clone(),
            store: self.store.clone(),
            path_map: self.path_map.clone(),
        }
    }

    /// Translate a daemon-local path under this review's project root to the
    /// same path on the machine (see `path_map`); anything else is unchanged.
    fn map_path(&self, path: &Path) -> PathBuf {
        match self.path_map.as_deref() {
            Some((local, remote)) => match path.strip_prefix(local) {
                Ok(suffix) if suffix.as_os_str().is_empty() => remote.clone(),
                Ok(suffix) => remote.join(suffix),
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
                    cwd: self.root.to_string_lossy().into_owned(),
                    program: String::new(),
                    args: vec![command.to_string()],
                    env: env.clone(),
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
                .with_provider(|p, spec| p.read_file(&full.to_string_lossy(), spec))
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
            Some(_) => {
                self.with_provider(|p, spec| p.write_file(&full.to_string_lossy(), content, spec))
            }
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
                p.write_executable_file(&full.to_string_lossy(), content, spec)
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
                    p.remove_path(&full.to_string_lossy(), recursive, spec)
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
                    &source.to_string_lossy(),
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
    fn resolve(&self, path: impl AsRef<Path>) -> PathBuf {
        let p = path.as_ref();
        if p.is_absolute() {
            self.map_path(p)
        } else {
            self.root.join(p)
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
        let req = crate::remote_runner::RunRequest {
            cwd: self.root.to_string_lossy().into_owned(),
            program: "git".to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            env: non_interactive_git_env(),
        };
        self.with_provider(|p, spec| p.run_vcs(&req, spec))
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
fn set_executable(path: &Path) -> Result<(), String> {
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
fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_artifact_materialization_publishes_an_exact_directory() {
        let root = std::env::temp_dir().join(format!(
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
            Workspace::local(std::env::temp_dir())
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
    fn a_local_workspace_actually_runs_git() {
        // Proves the local path is a real passthrough, not a stub.
        let ws = Workspace::local(std::env::temp_dir());
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
        let dir = std::env::temp_dir().join(format!("ral185-ws-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("ral185-wsfail-{}", std::process::id()));
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

    fn write_script(dir: &Path, json: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let (script, body) = if cfg!(windows) {
            (dir.join("p.cmd"), format!("@echo off\r\necho {json}\r\n"))
        } else {
            (dir.join("p.sh"), format!("#!/bin/sh\necho '{json}'\n"))
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
        let dir = std::env::temp_dir().join(format!("ral386-ws-retire-{}", std::process::id()));
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
