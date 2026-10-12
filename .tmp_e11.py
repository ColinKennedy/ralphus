import re
p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
s=s.replace('''/// If `wt`'s checked-out branch tracks a remote (its tracking config names a remote other than `.`
/// ), fetch that remote branch''','''/// If `wt`'s checked-out branch tracks a remote (its tracking config names a
/// remote other than `.`), fetch that remote branch''')
s=s.replace('''/// materialized from a `ralphus:new-worktree/<remote>/<branch>` placeholder
/// picks up''','''/// materialized from a `ralphus:new-worktree/<branch>?upstream_remote=...`
/// placeholder picks up''')
i=s.index("/// The `(branch, upstream)` a slot's creation marker names")
j=s.index("/// Every in-progress (or interrupted) creation",i)
s=s[:i]+'''/// The `(branch, upstream)` a slot's creation marker names, `None` when there
/// is no marker (or it is unreadable, which is treated the same: no proof of
/// ownership). The upstream comes from the marker's `upstream_remote=` and
/// `upstream_branch=` lines; a marker with only the combined `upstream=` line
/// is taken apart against the remotes configured in `root`.
fn read_creation_marker(root: &Path, path: &Path) -> Option<(String, Option<UpstreamRef>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let branch = lines.next()?.trim();
    if branch.is_empty() {
        return None;
    }
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let upstream = match (value("upstream_remote="), value("upstream_branch=")) {
        (Some(remote), Some(branch)) => Some(UpstreamRef::new(remote, branch)),
        _ => value("upstream=").map(|u| crate::legacy_upstream::split_legacy_base(&u, root)),
    };
    Some((branch.to_string(), upstream))
}

'''+s[j:]
s=s.replace("let (branch, _) = read_creation_marker(&entry.path())?;","let (branch, _) = read_creation_marker(root, &entry.path())?;")
s=s.replace('''    branch: &str,
    upstream: &str,
) -> Result<bool, String> {
    use std::io::Write as _;''','''    branch: &str,
    upstream: &UpstreamRef,
) -> Result<bool, String> {
    use std::io::Write as _;''')
s=s.replace('''            "{branch}
upstream={upstream}
pid={}",
            std::process::id()''','''            "{branch}
upstream_remote={}
upstream_branch={}
pid={}",
            upstream.remote,
            upstream.branch,
            std::process::id()''')
s=s.replace("Ok(read_creation_marker(&path).is_some_and(","Ok(read_creation_marker(root, &path).is_some_and(")
s=s.replace("read_creation_marker(&marker).is_some_and(","read_creation_marker(root, &marker).is_some_and(")
s=s.replace("read_creation_marker(&creation_marker_path(&root, short))?","read_creation_marker(&root, &creation_marker_path(&root, short))?")
s=s.replace("read_creation_marker(&creation_marker_path(&repo, \"slot\"))","read_creation_marker(&repo, &creation_marker_path(&repo, \"slot\"))")
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
