p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
s=s.replace("            upstream: Some(upstream.to_string()),\n","            upstream_remote: Some(upstream.remote.clone()),\n            upstream_branch: Some(upstream.branch.clone()),\n")
s=s.replace('''                            "upstream": upstream,
                            "error": e,''','''                            "upstream_remote": upstream.remote,
                            "upstream_branch": upstream.branch,
                            "error": e,''')
i=s.index("fn pin_remote_upstream_and_baseline(")
j=s.index("    workspace.git(&[\"config\", &format!(\"branch.{branch}.remote\"), &remote])?;",i)
s=s[:i]+'''fn pin_remote_upstream_and_baseline(
    workspace: &crate::workspace::Workspace,
    branch: &str,
    upstream: &UpstreamRef,
) -> Result<(), String> {
    // The provisioned workspace is a fresh clone whose only remote is
    // `origin`, whatever the daemon-local remote is called, so a remote base
    // is looked up there; a local base is looked up as a local branch.
    let candidates = if upstream.is_local() {
        vec![upstream.tracking_ref()]
    } else {
        vec![
            format!("refs/remotes/origin/{}", upstream.branch),
            upstream.tracking_ref(),
        ]
    };
    let qualified = candidates
        .iter()
        .find(|c| {
            workspace
                .git(&["rev-parse", "--verify", "--quiet", c])
                .is_ok()
        })
        .cloned()
        .ok_or_else(|| {
            format!(
                "upstream remote \\"{}\\" branch \\"{}\\" resolves to no ref in the workspace",
                upstream.remote, upstream.branch
            )
        })?;
    let (remote, merge) = if qualified.starts_with("refs/remotes/") {
        (
            "origin".to_string(),
            format!("refs/heads/{}", upstream.branch),
        )
    } else {
        (".".to_string(), qualified.clone())
    };
'''+s[j:]
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
