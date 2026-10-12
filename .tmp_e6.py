p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
i=s.index("pub(crate) fn resolve_registered_remote_upstream(")
j=s.index("/// The repository's default branch for `root`",i)
new='''pub(crate) fn resolve_registered_remote_upstream(
    root: &Path,
    project: &ProjectView,
    upstream: &UpstreamRef,
) -> Result<UpstreamRef, String> {
    if project.clone_url.is_none() || upstream.is_local() {
        return Ok(upstream.clone());
    }
    let remote = &upstream.remote;
    if git(root, &["remote", "get-url", remote]).is_err() {
        return Ok(upstream.clone());
    }
    let branch = &upstream.branch;
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    git(root, &["fetch", "--no-write-fetch-head", remote, &refspec]).map_err(|e| {
        format!(
            "project \\"{}\\" is registered with a remote, but upstream remote \\"{remote}\\" branch \\"{branch}\\" could not \
             be fetched: {e}",
            project.name
        )
    })?;
    Ok(upstream.clone())
}

'''
s=s[:i]+new+s[j:]
s=s.replace("use crate::guardian_merge::{git, qualify_ambiguous_ref};","use crate::guardian_merge::{git, qualify_upstream_ref};\nuse ralphus_core::schema::UpstreamRef;")
s=s.replace("HashMap<(String, String), String>","HashMap<(String, UpstreamRef), UpstreamRef>")
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
