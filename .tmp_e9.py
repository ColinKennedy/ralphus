p='daemon/src/reviews.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
i=s.index("/// `baseline` is qualified via [`crate::guardian_merge::qualify_ambiguous_ref`]")
j=s.index("/// Run a `git config` write against the shared",i)
s=s[:i]+'''/// `baseline` is qualified via [`crate::guardian_merge::qualify_upstream_ref`]
/// before resolution, so a remote base freezes to its remote-tracking ref and
/// a local base to its `refs/heads/` ref rather than to whichever same-named
/// ref git's DWIM search would pick.
pub(crate) fn set_worktree_commit_baseline(
    cwd: &Path,
    baseline: impl Into<UpstreamRef>,
) -> std::result::Result<(), String> {
    let branch = worktree_branch(cwd)?;
    let qualified = crate::guardian_merge::qualify_upstream_ref(cwd, &baseline.into());
    let resolved = git(cwd, &["rev-parse", "--verify", &qualified])?;
    git_config_write(
        cwd,
        &[
            "config",
            &format!("ralphus.{branch}.baseline"),
            resolved.trim(),
        ],
    )
}

'''+s[j:]
i=s.index("/// The upstream (`branch@{upstream}`) of the worktree's branch, if any.")
j=s.index("/// The durable comparison ref",i)
s=s[:i]+'''/// The upstream the worktree's branch tracks, read from its
/// `branch.<name>.remote` and `branch.<name>.merge` git config keys.
pub(crate) fn worktree_upstream(cwd: &Path) -> std::result::Result<UpstreamRef, String> {
    let branch = worktree_branch(cwd).map_err(|_| "no upstream".to_string())?;
    let read = |key: &str| {
        git(cwd, &["config", "--get", &format!("branch.{branch}.{key}")])
            .map(|v| v.trim().to_string())
            .ok()
            .filter(|v| !v.is_empty())
    };
    let (Some(remote), Some(merge)) = (read("remote"), read("merge")) else {
        return Err("no upstream".to_string());
    };
    let merge_branch = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
    Ok(UpstreamRef::new(remote, merge_branch))
}

'''+s[j:]
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
