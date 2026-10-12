def edit(p,f):
    s=open(p,encoding='utf-8',newline='').read()
    crlf='\r\n' in s
    s=s.replace('\r\n','\n')
    s=f(s)
    if crlf: s=s.replace('\n','\r\n')
    open(p,'w',encoding='utf-8',newline='').write(s)
def core(s):
    m='/// Is `remote` an acceptable upstream remote value'
    add='''/// The joined `<remote>/<branch>` display form of an upstream pair (just
/// `<branch>` for the local remote `.`). Display and key material only; the
/// pair itself is the source of truth.
#[must_use]
pub fn join_upstream(remote: &str, branch: &str) -> String {
    if remote == LOCAL_UPSTREAM_REMOTE {
        branch.to_string()
    } else {
        format!("{remote}/{branch}")
    }
}

'''
    return s.replace(m,add+m,1)
edit('core/src/schema.rs',core)
def wt(s):
    s=s.replace('''            let Some(upstream) =
                ralphus_core::schema::parse_worktree_placeholder_upstream(placeholder)
            else {
                continue;
            };
            if upstream.starts_with("<<") || upstream.contains('/') {
                continue;
            }''','''            let Some(upstream) =
                ralphus_core::schema::parse_worktree_placeholder_upstream(placeholder)
            else {
                continue;
            };
            if upstream.branch.starts_with("<<")
                || upstream.remote == ralphus_core::schema::LOCAL_UPSTREAM_REMOTE
            {
                continue;
            }''')
    return s
edit('daemon/src/worktrees.rs',wt)
