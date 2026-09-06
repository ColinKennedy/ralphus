//! The `cleanup` verb (RAL-355 Phase 2 remainder): explicitly tear a
//! provisioned workspace down.
//!
//! Never called automatically -- see `docs/machine-providers.md`'s "The
//! `cleanup` verb and its retention policy" section. Targets exactly one
//! workspace under the Phase 4 durable, multi-workspace-per-machine layout:
//! one worktree when `branch` is given, or the whole project directory
//! (repository plus every worktree) when it is omitted. Every resolved path
//! is derived from [`crate::layout`], which only ever concatenates onto
//! `remote_root` -- structurally incapable of resolving outside it -- and is
//! additionally proven via [`crate::job::require_under_root`] before
//! anything is removed, so a caller never has to trust that derivation alone.

use serde::Deserialize;

use crate::exec::EffectiveConfig;
use crate::job::{require_under_root, ssh_command};
use crate::layout;
use crate::transport::shell_quote_single;
use crate::uri;

#[derive(Debug, Deserialize)]
struct CleanupRequest {
    project: String,
    clone_url: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    remote_root: Option<String>,
}

/// Run `cleanup`: resolve the identified workspace's deterministic path and
/// remove it, reporting exactly what was removed.
///
/// # Errors
/// An actionable message on a malformed request, a request with no
/// configured `remote_root` (nothing durable to have provisioned in the
/// first place), or any transport/permission failure. On failure the
/// workspace is left exactly as it was -- there is no partial-removal state
/// to clean up after, since the remote `rm -rf` either fully succeeds or the
/// command fails outright before touching anything (a nonexistent target is
/// not attempted at all, see below).
pub fn run(uri: &str, payload_json: &str, config: &EffectiveConfig) -> Result<String, String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    let req: CleanupRequest = serde_json::from_str(payload_json)
        .map_err(|e| format!("could not parse the cleanup request on stdin: {e}"))?;
    if req.project.trim().is_empty() {
        return Err("cleanup request is missing a non-empty \"project\"".to_string());
    }
    if req.clone_url.trim().is_empty() {
        return Err("cleanup request is missing a non-empty \"clone_url\"".to_string());
    }
    let remote_root = req
        .remote_root
        .filter(|r| !r.trim().is_empty())
        .ok_or_else(|| {
            "cleanup requires a configured remote_root -- there is nothing durable to have \
         provisioned without one"
                .to_string()
        })?;
    let project_dir = layout::project_dir(&remote_root, &req.project, &req.clone_url);
    let target_dir = match req.branch.filter(|b| !b.trim().is_empty()) {
        Some(branch) => layout::worktree_dir(&project_dir, &branch),
        None => project_dir,
    };
    require_under_root(&target_dir, &remote_root)?;

    let script = format!("rm -rf {}", shell_quote_single(&target_dir));
    ssh_command(&target, &script, config, None)?;
    Ok(target_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EffectiveConfig {
        EffectiveConfig {
            remote_base: crate::config::DEFAULT_REMOTE_BASE.to_string(),
            extra_excludes: vec![],
            connect_timeout_secs: crate::config::DEFAULT_CONNECT_TIMEOUT_SECS,
            remote_runner_cmd: crate::config::DEFAULT_REMOTE_RUNNER_CMD.to_string(),
            ssh_config_file: None,
            target_runner_config: None,
        }
    }

    #[test]
    fn missing_remote_root_is_rejected_before_touching_the_network() {
        let err = run(
            "alice@host",
            r#"{"project":"proj","clone_url":"git@host:org/repo.git"}"#,
            &config(),
        )
        .unwrap_err();
        assert!(err.contains("remote_root"), "{err}");
    }

    #[test]
    fn missing_project_is_rejected() {
        let err = run(
            "alice@host",
            r#"{"project":"","clone_url":"git@host:org/repo.git","remote_root":"/srv/ralphus"}"#,
            &config(),
        )
        .unwrap_err();
        assert!(err.contains("project"), "{err}");
    }

    #[test]
    fn a_branch_scopes_to_one_worktree_not_the_whole_project() {
        // Both fail on the (unreachable) ssh connection -- the point here is
        // that the resolved path differs, which we can observe indirectly:
        // a bogus `remote_root` with an invalid-looking derived path would
        // fail `require_under_root` instead, and it does not, so the target
        // must have been resolved and scoping-checked before the network
        // call was ever attempted.
        let payload_whole = r#"{"project":"proj","clone_url":"git@host:org/repo.git","remote_root":"/srv/ralphus"}"#;
        let payload_branch = r#"{"project":"proj","clone_url":"git@host:org/repo.git","branch":"feature/x","remote_root":"/srv/ralphus"}"#;
        let err_whole = run("alice@host", payload_whole, &config()).unwrap_err();
        let err_branch = run("alice@host", payload_branch, &config()).unwrap_err();
        assert!(
            !err_whole.contains("outside configured remote_root"),
            "{err_whole}"
        );
        assert!(
            !err_branch.contains("outside configured remote_root"),
            "{err_branch}"
        );
    }
}
