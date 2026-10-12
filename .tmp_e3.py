import re
def edit(p,f):
    s=open(p,encoding='utf-8',newline='').read()
    crlf='\r\n' in s
    s=s.replace('\r\n','\n')
    s=f(s)
    if crlf: s=s.replace('\n','\r\n')
    open(p,'w',encoding='utf-8',newline='').write(s)
def core(s):
    m='/// Is `remote` an acceptable upstream remote value'
    add='''/// An owned upstream: a remote (or [`LOCAL_UPSTREAM_REMOTE`] for a local
/// branch) plus a literal branch on it. Never split from or joined into a
/// single string except for display.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct UpstreamRef {
    /// Remote name, or `.` for a local branch.
    pub remote: String,
    /// Branch on that remote (the local branch when the remote is `.`).
    pub branch: String,
}

impl UpstreamRef {
    /// Build a pair from its two halves.
    #[must_use]
    pub fn new(remote: impl Into<String>, branch: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            branch: branch.into(),
        }
    }

    /// A local-branch upstream (remote `.`).
    #[must_use]
    pub fn local(branch: impl Into<String>) -> Self {
        Self::new(LOCAL_UPSTREAM_REMOTE, branch)
    }

    /// Does this name a local branch rather than a remote's?
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.remote == LOCAL_UPSTREAM_REMOTE
    }

    /// The full ref this pair names: `refs/heads/<branch>` for a local base,
    /// else `refs/remotes/<remote>/<branch>`.
    #[must_use]
    pub fn tracking_ref(&self) -> String {
        if self.is_local() {
            format!("refs/heads/{}", self.branch)
        } else {
            format!("refs/remotes/{}/{}", self.remote, self.branch)
        }
    }

    /// The joined display string (see [`join_upstream`]).
    #[must_use]
    pub fn joined(&self) -> String {
        join_upstream(&self.remote, &self.branch)
    }
}

'''
    return s.replace(m,add+m,1)
edit('core/src/schema.rs',core)
def wt(s):
    a=s.index('/// A worktree placeholder\'s upstream after sentinel expansion')
    b=s.index('/// For a project registered with a remote')
    s=s[:a]+s[b:]
    s=s.replace('ResolvedUpstream','ralphus_core::schema::UpstreamRef')
    s=s.replace('''    Ok(ralphus_core::schema::UpstreamRef {
        remote: remote.to_string(),
        branch,
    })''','''    Ok(ralphus_core::schema::UpstreamRef::new(remote, branch))''')
    return s
edit('daemon/src/worktrees.rs',wt)
