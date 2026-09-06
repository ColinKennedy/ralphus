//! The `provision` verb: durable, deterministic Git clone/fetch/worktree
//! provisioning on the remote host (RAL-355 Phase 4).
//!
//! Mirrors the daemon's own local worktree pattern
//! (`daemon/src/worktrees.rs::ensure_worktree`) as closely as the remote
//! boundary allows: one persistent clone per project per machine, reused
//! and fetched (never re-cloned) on later calls, with `git worktree add`
//! creating each task branch's linked worktree off of it. See
//! `REMOTE_IMPROVEMENTS.local.md`'s Phase 2/4 design-interview notes for why
//! this deliberately does *not* pin an immutable base object ID the way the
//! plan's own aspirational language originally suggested -- ref-based
//! resolution matches what the local path does today, and pinning exact
//! SHAs is a robustness increment better done as dedicated follow-up than
//! folded into the first working version.
//!
//! Every value that originates outside this process (project name, clone
//! URL, branch, upstream, remote_root) is passed to the remote shell
//! through [`crate::transport::shell_quote_single`] -- never interpolated
//! raw. This is the one place in the whole provider where getting that
//! wrong would matter most: `provision` is the verb a hostile task file's
//! `project`/`branch` fields could otherwise reach a shell through.

use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::exec::EffectiveConfig;
use crate::layout;
use crate::ssh;
use crate::transport::shell_quote_single;
use crate::uri;

#[derive(Debug, Deserialize)]
struct WorkspaceSource {
    kind: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    upstream: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProvisionRequest {
    project: String,
    source: WorkspaceSource,
    #[serde(default)]
    remote_root: Option<String>,
    #[serde(default)]
    runner: Option<crate::runner_install::RunnerPolicy>,
}

/// Run `provision`: parse the request on stdin, ensure the deterministic
/// persistent clone/worktree exist on `target`, and return the absolute
/// remote worktree path.
///
/// # Errors
/// An actionable message on invalid input, a missing required field, an
/// unsupported VCS kind, an origin-identity mismatch, or any transport/git
/// failure. Never returns `Ok` for a workspace that isn't actually ready to
/// use.
pub fn run(uri: &str, request_json: &str, config: &EffectiveConfig) -> Result<String, String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    let req: ProvisionRequest = serde_json::from_str(request_json)
        .map_err(|e| format!("could not parse the provision request on stdin: {e}"))?;

    if req.source.kind != "git" {
        return Err(format!(
            "ralphus-ssh-provider's provision only supports git projects today; got kind {:?}",
            req.source.kind
        ));
    }
    let url = req
        .source
        .url
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            "provision request for a git project is missing a non-empty source.url".to_string()
        })?;
    let branch = req
        .source
        .branch
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "provision request is missing the worktree branch to create".to_string())?;
    let upstream = req
        .source
        .upstream
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            "provision request is missing the upstream/base reference to create the branch from"
                .to_string()
        })?;
    let remote_root = req.remote_root.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
        "provision request has no remote_root -- register a [machine.targets.*] entry for this machine with one set".to_string()
    })?;
    if let Some(policy) = &req.runner {
        if policy.remote_root != remote_root {
            return Err(
                "runner policy remote_root does not match provision remote_root".to_string(),
            );
        }
        crate::runner_install::ensure(&target, policy, config)?;
    }

    let project_dir = layout::project_dir(&remote_root, &req.project, &url);
    let repo_dir = layout::repository_dir(&project_dir);
    let worktrees_parent = layout::worktrees_parent_dir(&project_dir);
    let worktree_dir = layout::worktree_dir(&project_dir, &branch);
    let lock_path = layout::lock_path(&project_dir);
    let metadata_path = layout::metadata_path(&project_dir);

    let script = build_provision_script(BuildScriptArgs {
        project_dir: &project_dir,
        repo_dir: &repo_dir,
        worktrees_parent: &worktrees_parent,
        worktree_dir: &worktree_dir,
        lock_path: &lock_path,
        metadata_path: &metadata_path,
        project_name: &req.project,
        url: &url,
        branch: &branch,
        upstream: &upstream,
    });

    let ssh_args = ssh::command_args(
        &target.target_string(),
        config.connect_timeout_secs,
        &script,
        config.ssh_config_file.as_deref(),
    );
    let out = Command::new("ssh")
        .args(&ssh_args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run ssh to reach {target}: {e}"))?;

    if !out.status.success() {
        let raw_stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = redact_url_from_error_text(&raw_stderr, &url);
        // The script's own explicit failures (origin mismatch, branch
        // validation) are written to stderr with a stable prefix so they
        // surface verbatim rather than being reinterpreted as a generic ssh
        // failure -- see `build_provision_script`'s `fail()` helper.
        if let Some(reason) = stderr
            .lines()
            .find_map(|l| l.strip_prefix("ralphus-ssh-provider: provision failed: "))
        {
            return Err(format!("provision failed on {target}: {reason}"));
        }
        return Err(ssh::interpret_failure("ssh", out.status.code(), &stderr));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let workspace = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(str::trim)
        .ok_or_else(|| {
            format!("provision on {target} produced no output; expected the remote worktree path")
        })?;
    if workspace != worktree_dir {
        // The script always echoes exactly the path we asked it to prepare;
        // anything else means it didn't reach the end of the script cleanly
        // even though its exit code was 0 (e.g. `set -e` didn't trip on
        // something it should have) -- treat that as untrustworthy rather
        // than returning a workspace we can't vouch for.
        return Err(format!(
            "provision on {target} produced an unexpected workspace path {workspace:?} (expected {worktree_dir:?})"
        ));
    }
    Ok(workspace.to_string())
}

/// Scrub `url` out of `error_text`, replacing it with its credential-redacted
/// form (RAL-355 Phase 1's `redact_url_credentials`) wherever it appears
/// verbatim. A git failure (a private repo this account can't authenticate
/// to, most commonly) can echo the clone URL straight into its own error
/// text -- if that URL carries inline HTTP(S) credentials, those must never
/// reach the daemon's error string, a Cartographer payload, or a human's
/// terminal. A no-op for any URL form `redact_url_credentials` itself
/// no-ops on (SSH, `file://`, a bare HTTPS URL with no embedded password) --
/// see that function's own doc for why only `scheme://user:pass@host` shapes
/// are ever rewritten.
#[must_use]
fn redact_url_from_error_text(error_text: &str, url: &str) -> String {
    if error_text.contains(url) {
        error_text.replace(url, &ralphus_core::redact::redact_url_credentials(url))
    } else {
        error_text.to_string()
    }
}

struct BuildScriptArgs<'a> {
    project_dir: &'a str,
    repo_dir: &'a str,
    worktrees_parent: &'a str,
    worktree_dir: &'a str,
    lock_path: &'a str,
    metadata_path: &'a str,
    project_name: &'a str,
    url: &'a str,
    branch: &'a str,
    upstream: &'a str,
}

/// Normalize a resolved upstream reference into the branch name the fresh
/// remote clone's `origin` remote should fetch/track. The daemon's own
/// `resolve_upstream` can hand back either a bare branch name (`"main"`) or
/// an already-`<remote>/<branch>` form (`"origin/main"`) naming *its own*
/// local checkout's remote alias -- which is not necessarily called
/// `origin` there, but this provider's fresh clone always names its remote
/// `origin` (`git clone --origin origin`), so only the branch half of a
/// slash-qualified upstream is meaningful here.
fn upstream_branch_name(upstream: &str) -> &str {
    match upstream.split_once('/') {
        Some((_, branch)) if !branch.is_empty() => branch,
        _ => upstream,
    }
}

/// Build the POSIX `sh` script run on the remote host to provision one
/// worktree. Every caller-supplied value is quoted with
/// [`shell_quote_single`] before interpolation -- see the module doc.
fn build_provision_script(args: BuildScriptArgs<'_>) -> String {
    let project_dir_q = shell_quote_single(args.project_dir);
    let repo_dir_q = shell_quote_single(args.repo_dir);
    let worktrees_parent_q = shell_quote_single(args.worktrees_parent);
    let worktree_dir_q = shell_quote_single(args.worktree_dir);
    let lock_path_q = shell_quote_single(args.lock_path);
    let metadata_path_q = shell_quote_single(args.metadata_path);
    let project_name_q = shell_quote_single(args.project_name);
    let url_q = shell_quote_single(args.url);
    let branch_q = shell_quote_single(args.branch);
    let origin_ref_q =
        shell_quote_single(&format!("origin/{}", upstream_branch_name(args.upstream)));
    let local_branch_ref_q = shell_quote_single(&format!("refs/heads/{}", args.branch));
    // Cloned into a staging directory and `mv`'d into place only once the
    // whole clone succeeds, so an interrupted connection (or a killed
    // provider process) mid-clone never leaves a directory at `repo_dir`
    // that a later call would mistake for "already cloned" -- `mv` within
    // the same filesystem (guaranteed here: both paths are under the same
    // `remote_root`) is atomic. Any stale staging directory from a prior
    // interrupted attempt is removed first, under the same lock, before
    // retrying.
    let staging_repo_dir_q = shell_quote_single(&format!("{}.tmp-clone", args.repo_dir));

    format!(
        r#"set -eu
fail() {{
    echo "ralphus-ssh-provider: provision failed: $1" >&2
    exit 1
}}
mkdir -p {project_dir_q} {worktrees_parent_q}
exec 9>{lock_path_q}
flock 9
if [ -d {repo_dir_q}/.git ]; then
    recorded_url=$(git -C {repo_dir_q} remote get-url origin 2>/dev/null) || fail "existing repository at {repo_dir_q} has no 'origin' remote"
    if [ "$recorded_url" != {url_q} ]; then
        fail "origin mismatch for {project_name_q}: expected {url_q} but {repo_dir_q} has origin $recorded_url -- a directory hash collision or manual tampering, refusing to adopt it"
    fi
    git -C {repo_dir_q} fetch origin
else
    rm -rf {staging_repo_dir_q}
    git clone --origin origin {url_q} {staging_repo_dir_q}
    mv {staging_repo_dir_q} {repo_dir_q}
    printf '{{"project": "%s", "url": "%s"}}\n' {project_name_q} {url_q} > {metadata_path_q}
fi
if [ ! -d {worktree_dir_q} ]; then
    if git -C {repo_dir_q} rev-parse --verify --quiet {local_branch_ref_q} >/dev/null; then
        git -C {repo_dir_q} worktree add {worktree_dir_q} {branch_q}
    else
        git -C {repo_dir_q} worktree add --track -b {branch_q} {worktree_dir_q} {origin_ref_q}
    fi
fi
echo {worktree_dir_q}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn args<'a>(
        project_dir: &'a str,
        repo_dir: &'a str,
        worktrees_parent: &'a str,
        worktree_dir: &'a str,
        lock_path: &'a str,
        metadata_path: &'a str,
        project_name: &'a str,
        url: &'a str,
        branch: &'a str,
        upstream: &'a str,
    ) -> BuildScriptArgs<'a> {
        BuildScriptArgs {
            project_dir,
            repo_dir,
            worktrees_parent,
            worktree_dir,
            lock_path,
            metadata_path,
            project_name,
            url,
            branch,
            upstream,
        }
    }

    #[test]
    fn upstream_branch_name_strips_a_remote_alias_prefix() {
        assert_eq!(upstream_branch_name("origin/main"), "main");
        assert_eq!(upstream_branch_name("main"), "main");
        assert_eq!(upstream_branch_name("some/nested/branch"), "nested/branch");
    }

    #[test]
    fn redact_url_from_error_text_scrubs_an_inline_password() {
        let url = "https://user:hunter2@example.invalid/team/proj.git";
        let error_text = format!("fatal: could not read from '{url}': Authentication failed");
        let scrubbed = redact_url_from_error_text(&error_text, url);
        assert!(!scrubbed.contains("hunter2"), "{scrubbed}");
        assert!(
            scrubbed.contains("https://***@example.invalid/team/proj.git"),
            "{scrubbed}"
        );
    }

    #[test]
    fn redact_url_from_error_text_is_a_noop_for_urls_with_no_embedded_credentials() {
        for url in [
            "git@example.invalid:team/proj.git",
            "file:///srv/git/ralphus-test.git",
            "https://example.invalid/team/proj.git",
        ] {
            let error_text = format!("fatal: repository '{url}' not found");
            assert_eq!(redact_url_from_error_text(&error_text, url), error_text);
        }
    }

    #[test]
    fn redact_url_from_error_text_is_a_noop_when_the_url_never_appears() {
        let error_text = "fatal: could not resolve host: example.invalid";
        assert_eq!(
            redact_url_from_error_text(
                error_text,
                "https://user:hunter2@example.invalid/team/proj.git"
            ),
            error_text
        );
    }

    #[test]
    fn build_provision_script_quotes_every_interpolated_value() {
        let script = build_provision_script(args(
            "/root/projects/p-1",
            "/root/projects/p-1/repository",
            "/root/projects/p-1/worktrees",
            "/root/projects/p-1/worktrees/w-1",
            "/root/projects/p-1/.provision.lock",
            "/root/projects/p-1/metadata.json",
            "it's a project",
            "https://example.invalid/a b.git",
            "feature/x; rm -rf /",
            "main",
        ));
        // A value containing a single quote must be escaped, never left
        // able to terminate the quoted string early.
        assert!(script.contains(r"it'\''s a project"), "{script}");
        // A shell-metacharacter-laden branch name must stay inside quotes,
        // never able to inject a second command -- every occurrence of the
        // dangerous substring in the whole script must be accounted for by
        // one of its two safely-quoted forms (the bare branch, and the
        // `refs/heads/<branch>` ref built from it), not merely present
        // *somewhere*, which a naive `contains` check on its own wouldn't
        // rule out an additional unquoted occurrence elsewhere.
        let danger_occurrences = script.matches("rm -rf /").count();
        let safely_quoted_occurrences = script.matches("'feature/x; rm -rf /'").count()
            + script.matches("'refs/heads/feature/x; rm -rf /'").count();
        assert!(danger_occurrences > 0, "{script}");
        assert_eq!(danger_occurrences, safely_quoted_occurrences, "{script}");
    }

    #[test]
    fn build_provision_script_uses_origin_as_the_remote_name_regardless_of_upstream_form() {
        let bare = build_provision_script(args(
            "/r/p",
            "/r/p/repository",
            "/r/p/worktrees",
            "/r/p/worktrees/w",
            "/r/p/.provision.lock",
            "/r/p/metadata.json",
            "proj",
            "https://example.invalid/x.git",
            "feat",
            "main",
        ));
        assert!(bare.contains("'origin/main'"), "{bare}");

        let qualified = build_provision_script(args(
            "/r/p",
            "/r/p/repository",
            "/r/p/worktrees",
            "/r/p/worktrees/w",
            "/r/p/.provision.lock",
            "/r/p/metadata.json",
            "proj",
            "https://example.invalid/x.git",
            "feat",
            "upstream/main",
        ));
        // The daemon-local remote alias name ("upstream") is discarded; this
        // provider's fresh clone's remote is always called "origin".
        assert!(qualified.contains("'origin/main'"), "{qualified}");
        assert!(!qualified.contains("upstream/main"), "{qualified}");
    }

    #[test]
    fn build_provision_script_checks_lock_before_any_git_command() {
        let script = build_provision_script(args(
            "/r/p",
            "/r/p/repository",
            "/r/p/worktrees",
            "/r/p/worktrees/w",
            "/r/p/.provision.lock",
            "/r/p/metadata.json",
            "proj",
            "https://example.invalid/x.git",
            "feat",
            "main",
        ));
        let lock_pos = script.find("flock 9").expect("flock present");
        let git_pos = script.find("git ").expect("git command present");
        assert!(
            lock_pos < git_pos,
            "lock must be acquired before any git command:\n{script}"
        );
    }

    #[test]
    fn build_provision_script_clones_into_a_staging_dir_and_renames_atomically() {
        // Regression against "an interrupted clone leaves a directory at
        // repo_dir that a later call mistakes for already-cloned": the
        // script must clone into a `.tmp-clone`-suffixed staging path and
        // only `mv` it into the real `repository` path after the clone
        // fully succeeds.
        let script = build_provision_script(args(
            "/r/p",
            "/r/p/repository",
            "/r/p/worktrees",
            "/r/p/worktrees/w",
            "/r/p/.provision.lock",
            "/r/p/metadata.json",
            "proj",
            "https://example.invalid/x.git",
            "feat",
            "main",
        ));
        let clone_pos = script.find("git clone").expect("clone command present");
        let mv_pos = script
            .find("mv '/r/p/repository.tmp-clone' '/r/p/repository'")
            .expect("an mv from the staging directory to the real repository path must be present");
        assert!(
            clone_pos < mv_pos,
            "clone must complete before the atomic rename:\n{script}"
        );
        assert!(
            script.contains("git clone --origin origin 'https://example.invalid/x.git' '/r/p/repository.tmp-clone'"),
            "clone must target the staging directory, not repo_dir directly:\n{script}"
        );
        assert!(
            script.contains("rm -rf '/r/p/repository.tmp-clone'"),
            "a stale staging directory from a prior interrupted attempt must be cleared first:\n{script}"
        );
    }

    #[test]
    fn build_provision_script_verifies_origin_identity_before_reusing_an_existing_clone() {
        let script = build_provision_script(args(
            "/r/p",
            "/r/p/repository",
            "/r/p/worktrees",
            "/r/p/worktrees/w",
            "/r/p/.provision.lock",
            "/r/p/metadata.json",
            "proj",
            "https://example.invalid/x.git",
            "feat",
            "main",
        ));
        assert!(script.contains("remote get-url origin"), "{script}");
        assert!(script.contains("origin mismatch"), "{script}");
    }

    #[test]
    fn build_provision_script_writes_syntactically_valid_metadata_json() {
        // Regression: an earlier version of this script wrote
        // `{"project": test-project, "url": file:///...}` -- the printf
        // format string's %s substitutions had no surrounding literal
        // double quotes, so the shell-quoted (single-quote) argument
        // values, once unquoted by the shell, landed in the file as bare
        // unquoted text instead of JSON string values. Verified live
        // against the real Docker SSH fixture, not just this string check.
        let script = build_provision_script(args(
            "/r/p",
            "/r/p/repository",
            "/r/p/worktrees",
            "/r/p/worktrees/w",
            "/r/p/.provision.lock",
            "/r/p/metadata.json",
            "proj",
            "https://example.invalid/x.git",
            "feat",
            "main",
        ));
        assert!(
            script.contains(r#"printf '{"project": "%s", "url": "%s"}\n'"#),
            "{script}"
        );
    }
}
