p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
i=s.index("fn set_explicit_upstream(wt: &Path, branch: &str, upstream: &str)")
j=s.index('    Err(fail("no matching ref found".to_string()))\n}\n', i)+len('    Err(fail("no matching ref found".to_string()))\n}\n')
BS=chr(92)
new='''fn set_explicit_upstream(wt: &Path, branch: &str, upstream: &UpstreamRef) -> Result<(), String> {
    let _guard = WORKTREE_CONFIG_LOCK.lock();
    // RAL-258: the reserved `<<...>>` sentinels are never literal branch names.
    // They must be expanded by `resolve_upstream` before materialization; a
    // literal `<<...>>` here means resolution was skipped, not a real ref to
    // track (a branch named `<<default>>` would be meaningless and ambiguous).
    if upstream.branch.starts_with("<<") {
        return Err(format!(
            "upstream_branch {} is a reserved sentinel and must be resolved to a concrete branch (via resolve_upstream) before materializing a worktree, not treated as a literal branch name",
            upstream.branch
        ));
    }
    let fail = |e: String| -> String {
        format!(
            "could not set upstream to remote {:?} branch {:?} for the worktree at {}: {e} -- the branch must already exist as a local branch (remote \\".\\") or a remote-tracking ref (fetch the remote first)",
            upstream.remote,
            upstream.branch,
            wt.display()
        )
    };
    git(wt, &["rev-parse", "--verify", &upstream.tracking_ref()]).map_err(fail)?;
    git_config_write_retrying(
        wt,
        &["config", &format!("branch.{branch}.remote"), &upstream.remote],
    )
    .map_err(fail)?;
    git_config_write_retrying(
        wt,
        &[
            "config",
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{}", upstream.branch),
        ],
    )
    .map_err(fail)?;
    Ok(())
}
'''.replace('\\\\"', BS+'"')
s=s[:i]+new+s[j:]
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
