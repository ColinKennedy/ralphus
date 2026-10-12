p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
i=s.index("/// If no local branch by that literal name exists but a remote-tracking ref\n/// `refs/remotes/<branch>`")
j=s.index("pub fn ensure_worktree(",i)
s=s[:i]+'''/// The branch name is always taken literally -- slashes included -- and a new
/// local branch is forked from the resolved `upstream` ref itself, **never
/// from `root`'s current `HEAD`**: whatever the shared project checkout
/// happens to have checked out at materialization time must never leak into a
/// freshly created branch's history. `upstream` -- not the branch's own name
/// or `root`'s `HEAD` -- decides both what the branch actually starts from and
/// what tracking gets configured; a review worktree tracking a remote keeps up
/// with pushes to that branch over the life of the project via the resync
/// described above.
'''+s[j:]
s=s.replace("if !seen.insert((project_name.to_string(), upstream.to_string())) {","let upstream = UpstreamRef::new(upstream.remote, upstream.branch);\n            if !seen.insert((project_name.to_string(), upstream.clone())) {")
s=s.replace("            out.push((project, upstream.to_string()));","            out.push((project, upstream));")
s=s.replace('''    branch: &str,
    upstream: &str,
    snapshot: &mut HashMap<String, String>,''','''    branch: &str,
    upstream: &UpstreamRef,
    snapshot: &mut HashMap<String, String>,''')
s=s.replace("    pub(crate) upstream: String,\n    pub(crate) wt: PathBuf,","    pub(crate) upstream: UpstreamRef,\n    pub(crate) wt: PathBuf,")
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
