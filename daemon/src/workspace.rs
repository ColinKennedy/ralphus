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
use std::sync::{Arc, Mutex};

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
    store: Option<Arc<Mutex<Store>>>,
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
        store: &Arc<Mutex<Store>>,
        guardian_id: &str,
        root: impl Into<PathBuf>,
    ) -> Self {
        let machine = store
            .lock()
            .expect("poisoned")
            .get_guardian(guardian_id)
            .ok()
            .and_then(|g| g.machine);
        Self::on(root, machine.as_deref()).with_store(Arc::clone(store))
    }

    /// Attach the store handle a remote workspace needs to resolve its
    /// provider. A no-op in effect for a local workspace, which never looks.
    #[must_use]
    pub fn with_store(mut self, store: Arc<Mutex<Store>>) -> Self {
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
    /// [`crate::verify::run_command_verify_capture`] exactly as before; remote
    /// ones send the command to the provider.
    ///
    /// Distinct from [`Self::git`] because a check gate is an arbitrary shell
    /// command (`cargo test`, `npm run build`), not a VCS operation — the
    /// provider may well want to run the two very differently.
    #[must_use]
    pub fn run_command(&self, command: &str) -> (bool, String) {
        match &self.machine {
            None => crate::verify::run_command_verify_capture(
                &self.root.to_string_lossy(),
                command,
                &opentelemetry::Context::new(),
                &std::collections::BTreeMap::new(),
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
                };
                match self.with_provider(|p, spec| p.run_vcs(&req, spec)) {
                    Ok(out) => (true, out),
                    Err(e) => (false, e),
                }
            }
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
            let guard = store.lock().expect("poisoned");
            crate::remote_runner::provider_from_store(&guard, machine)?.ok_or_else(|| {
                format!("machine \"{machine}\" resolved to the local host unexpectedly")
            })?
        };
        // These operations belong to no session; the spec exists only to
        // satisfy the invocation shape.
        let spec = crate::runner::RunnerSpec::for_command_verify(
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
        };
        self.with_provider(|p, spec| p.run_vcs(&req, spec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
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
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        store
            .lock()
            .unwrap()
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
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        store
            .lock()
            .unwrap()
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
}
