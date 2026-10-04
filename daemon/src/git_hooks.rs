//! RAL-445/RAL-531 commit metadata hook installation.
//!
//! The managed `prepare-commit-msg` hook adds the optional Ralphus co-author
//! trailer and associates cell-authored commits with a durable `Ralphus-Cell:`
//! trailer. Syncing uses [`Workspace`] so the same hook is installed in local
//! and remote worktrees.

use std::path::PathBuf;

use crate::workspace::Workspace;

/// Marker embedded in every hook this module installs. It prevents a sync from
/// overwriting a hand-authored `prepare-commit-msg` hook.
const MARKER: &str = "# ralphus:coauthor-hook (RAL-445)";

/// Environment variable exported for a cell's agent process. The hook reads
/// it at commit time to attach the stable cell URI.
pub const ENTITY_URI_ENV: &str = "RALPHUS_ENTITY_URI";

/// Git trailer key that associates a commit with the cell that authored it.
pub const CELL_TRAILER: &str = "Ralphus-Cell";

/// The exact trailer value every co-author-enabled project's commits receive.
pub const COAUTHOR_TRAILER: &str = "ralphus-bot <ralphus-bot@users.noreply.github.com>";

/// The hook script installed at `<hooks dir>/prepare-commit-msg`.
fn hook_script(add_coauthor: bool) -> String {
    let coauthor = if add_coauthor {
        format!(
            "exec git interpret-trailers --in-place --if-exists addIfDifferent \\\n             \t--trailer \"Co-authored-by: {COAUTHOR_TRAILER}\" \"$1\"\n"
        )
    } else {
        "exit 0\n".to_string()
    };
    format!(
        "#!/bin/sh\n\
         {MARKER} -- managed by ralphus, do not edit by hand.\n\
         # Re-running ralphus against this project regenerates this file; hand edits\n\
         # made here are lost the next time it syncs.\n\
         if [ -n \"${{{ENTITY_URI_ENV}:-}}\" ]; then\n\
         \tgit interpret-trailers --in-place --if-exists addIfDifferent \\\n         \t\t--trailer \"{CELL_TRAILER}: ${ENTITY_URI_ENV}\" \"$1\"\n\
         fi\n\
         {coauthor}"
    )
}

/// Resolve the effective Git hooks directory, including a configured
/// `core.hooksPath`.
fn hooks_dir(workspace: &Workspace) -> Result<PathBuf, String> {
    let out = workspace.git(&["rev-parse", "--git-path", "hooks"])?;
    let value = out.trim();
    if value.is_empty() {
        return Err("git rev-parse --git-path hooks returned empty output".to_string());
    }
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        workspace.root().join(path)
    })
}

fn is_ralphus_managed(workspace: &Workspace, path: &std::path::Path) -> bool {
    workspace
        .read_file(path)
        .is_some_and(|contents| contents.contains(MARKER))
}

/// Install the RAL-445/RAL-531 commit metadata hook for `workspace`.
///
/// The sync is idempotent. A `prepare-commit-msg` hook without [`MARKER`]
/// belongs to another tool and is left untouched. `add_coauthor` controls the
/// optional co-author trailer; cell association is installed regardless.
pub fn sync_commit_metadata_hook(workspace: &Workspace, add_coauthor: bool) -> Result<(), String> {
    let dir = hooks_dir(workspace)?;
    let hook_path = dir.join("prepare-commit-msg");
    if workspace.read_file(&hook_path).is_some() && !is_ralphus_managed(workspace, &hook_path) {
        // ralphus[ignore-rlog-pair]: hook installer has no Store and runs once per worktree resolution; caller owns structured outcomes
        crate::rlog!(
            DEBUG,
            "ralphus [worktrees] leaving hand-authored prepare-commit-msg hook at {} untouched; \
             commits there will not get the {CELL_TRAILER} trailer",
            hook_path.display()
        );
        return Ok(());
    }
    workspace.write_executable_file(&hook_path, &hook_script(add_coauthor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian_merge::git;
    use std::path::Path;
    use std::process::Command;

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "--quiet"]).unwrap();
        git(dir, &["config", "user.email", "test@example.com"]).unwrap();
        git(dir, &["config", "user.name", "Test"]).unwrap();
    }

    fn temp_repo(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ralphus-git-hooks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init_repo(&dir);
        dir
    }

    fn git_with_cell_uri(repo: &Path, args: &[&str], uri: &str) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env(ENTITY_URI_ENV, uri)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn sync_attaches_cell_uri_without_corrupting_existing_trailers() {
        let repo = temp_repo("cell-uri");
        let workspace = Workspace::local(&repo);
        sync_commit_metadata_hook(&workspace, true).unwrap();

        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        git(&repo, &["add", "a.txt"]).unwrap();
        git_with_cell_uri(
            &repo,
            &[
                "commit",
                "-m",
                "first commit\n\nReviewed-by: Someone <someone@example.com>",
            ],
            "cell:squad-1:0:0",
        );

        let message = git(&repo, &["log", "-1", "--format=%B"]).unwrap();
        assert!(message.contains("Reviewed-by: Someone <someone@example.com>"));
        assert!(message.contains("Ralphus-Cell: cell:squad-1:0:0"));
        assert!(message.contains(&format!("Co-authored-by: {COAUTHOR_TRAILER}")));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn sync_dedupes_a_cell_uri_and_excludes_unrelated_ones() {
        let repo = temp_repo("dedupe-cell-uri");
        let workspace = Workspace::local(&repo);
        sync_commit_metadata_hook(&workspace, true).unwrap();

        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        git(&repo, &["add", "a.txt"]).unwrap();
        git_with_cell_uri(&repo, &["commit", "-m", "first commit"], "cell:squad-1:0:0");
        git_with_cell_uri(
            &repo,
            &["commit", "--amend", "-m", "first commit, reworded"],
            "cell:squad-1:0:0",
        );

        let message = git(&repo, &["log", "-1", "--format=%B"]).unwrap();
        assert_eq!(message.matches("Ralphus-Cell: cell:squad-1:0:0").count(), 1);
        assert!(!message.contains("Ralphus-Cell: cell:squad-1:0:1"));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn coauthor_opt_out_keeps_cell_association() {
        let repo = temp_repo("cell-uri-no-coauthor");
        let workspace = Workspace::local(&repo);
        sync_commit_metadata_hook(&workspace, false).unwrap();

        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        git(&repo, &["add", "a.txt"]).unwrap();
        git_with_cell_uri(&repo, &["commit", "-m", "first commit"], "cell:squad-1:0:0");

        let message = git(&repo, &["log", "-1", "--format=%B"]).unwrap();
        assert!(message.contains("Ralphus-Cell: cell:squad-1:0:0"));
        assert!(!message.contains("Co-authored-by:"));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn sync_never_overwrites_a_foreign_hook() {
        let repo = temp_repo("foreign-hook");
        let workspace = Workspace::local(&repo);
        let hooks = hooks_dir(&workspace).unwrap();
        std::fs::create_dir_all(&hooks).unwrap();
        let hook_path = hooks.join("prepare-commit-msg");
        std::fs::write(&hook_path, "#!/bin/sh\necho not-ralphus\n").unwrap();

        sync_commit_metadata_hook(&workspace, true).unwrap();
        let contents = std::fs::read_to_string(&hook_path).unwrap();
        assert!(contents.contains("not-ralphus"));
        assert!(!contents.contains(MARKER));

        let _ = std::fs::remove_dir_all(&repo);
    }
}
