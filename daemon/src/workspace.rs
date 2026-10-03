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
        let machine = store
            .lock()
            .get_guardian(guardian_id)
            .ok()
            .and_then(|g| g.machine);
        Self::on(root, machine.as_deref()).with_store(Arc::clone(store))
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
            root: path.into(),
            machine: self.machine.clone(),
            store: self.store.clone(),
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
    /// **Local workspaces only.** A remote workspace's
    /// [`crate::remote_runner::RunRequest`] has no env field, so `env` is
    /// ignored there rather than silently half-applied; a check gate on a
    /// remote machine still runs exactly as it did before. `cancel` is
    /// likewise local-only (RAL-239): a remote provider call has no polling
    /// hook to kill mid-flight, so it blocks to completion same as before.
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
                if recursive {
                    let _ = std::fs::remove_dir_all(&full);
                } else {
                    let _ = std::fs::remove_file(&full);
                }
            }
            Some(_) => {
                let _ = self.with_provider(|p, spec| {
                    p.remove_path(&full.to_string_lossy(), recursive, spec)
                });
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
            p.to_path_buf()
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
    fn git_remote(&self, _machine: &str, args: &[&str]) -> Result<String, String> {
        let req = crate::remote_runner::RunRequest {
            cwd: self.root.to_string_lossy().into_owned(),
            program: "git".to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            env: std::collections::BTreeMap::new(),
        };
        self.with_provider(|p, spec| p.run_vcs(&req, spec))
    }
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
