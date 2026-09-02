use std::path::Path;
use std::process::Command;

pub fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "ralphus")
        .env("GIT_AUTHOR_EMAIL", "ralphus@example.com")
        .env("GIT_COMMITTER_NAME", "ralphus")
        .env("GIT_COMMITTER_EMAIL", "ralphus@example.com")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// Both integration suites need isolated repositories with identical Git
// identity and line-ending behavior. Sharing this immutable template is safe
// because `git init` copies its config into each test's independently created
// `.git`; repositories, object stores, refs, and linked worktrees are not shared.
pub fn init_repo(root: &Path) {
    let template = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/git-template");
    let template_arg = format!("--template={}", template.display());
    git(root, &["init", "-b", "main", &template_arg]);
}
