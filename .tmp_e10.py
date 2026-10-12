p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
i=s.index("fn freeze_commit_baseline(wt: &Path, branch: &str) {")
j=s.index("    // `set_worktree_commit_baseline` writes",i)
s=s[:i]+'''fn freeze_commit_baseline(wt: &Path, branch: &str) {
    let Ok(upstream) = crate::reviews::worktree_upstream(wt) else {
        return;
    };
'''+s[j:]
s=s.replace("crate::reviews::set_worktree_commit_baseline(wt, upstream.trim())","crate::reviews::set_worktree_commit_baseline(wt, upstream)")
i=s.index("fn resync_remote_tracking_branch(wt: &Path)")
j=s.index("    git(wt, &[\"fetch\", \"--no-write-fetch-head\", remote, &refspec])",i)
s=s[:i]+'''fn resync_remote_tracking_branch(wt: &Path) -> Result<(), String> {
    let Ok(tracked) = crate::reviews::worktree_upstream(wt) else {
        return Ok(());
    };
    if tracked.is_local() {
        return Ok(());
    }
    let remote = tracked.remote.as_str();
    let remote_branch = tracked.branch.as_str();
    let upstream = tracked.tracking_ref();
    let upstream = upstream.as_str();
    let refspec = format!("{remote_branch}:{upstream}");
'''+s[j:]
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
