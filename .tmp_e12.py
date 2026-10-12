p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
s=s.replace('''pub fn ensure_worktree(root: &Path, branch: &str, upstream: &str) -> Result<PathBuf, String> {
    ensure_worktree_tracked(root, branch, upstream).map(|m| m.path)''','''pub fn ensure_worktree(
    root: &Path,
    branch: &str,
    upstream: impl Into<UpstreamRef>,
) -> Result<PathBuf, String> {
    ensure_worktree_tracked(root, branch, &upstream.into()).map(|m| m.path)''')
s=s.replace('''fn materialize_exclusively(
    root: &Path,
    branch: &str,
    upstream: &str,''','''fn materialize_exclusively(
    root: &Path,
    branch: &str,
    upstream: &UpstreamRef,''')
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
