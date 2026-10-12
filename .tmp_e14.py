def edit(p, pairs):
    s=open(p,encoding='utf-8',newline='').read()
    crlf='\r\n' in s
    s=s.replace('\r\n','\n')
    for a,b in pairs:
        assert s.count(a)==1,(p,a[:60],s.count(a))
        s=s.replace(a,b)
    if crlf: s=s.replace('\n','\r\n')
    open(p,'w',encoding='utf-8',newline='').write(s)
edit('daemon/src/remote_runner.rs',[('''    /// The already-resolved (RAL-100/RAL-258: no `?upstream=<<sentinel>>` or
    /// literal `<remote>/<branch>` ambiguity left) reference `branch` should
    /// be created from/track, when meaningful. Mirrors what
    /// `worktrees::ensure_worktree`'s own `upstream` parameter carries for a
    /// local worktree (RAL-355 Phase 4) -- until this was added, a
    /// `provision` request had no way to say what a brand-new remote branch
    /// should be based on at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
''','''    /// The remote half of the already-resolved (no `<<sentinel>>` left)
    /// upstream `branch` should be created from/track, when meaningful.
    /// `.` names a local base. Mirrors what `worktrees::ensure_worktree`'s own
    /// `upstream` parameter carries for a local worktree (RAL-355 Phase 4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_remote: Option<String>,
    /// The branch half of the resolved upstream, on `upstream_remote`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_branch: Option<String>,
''')])
edit('ssh-provider/src/provision.rs',[('''    #[serde(default)]
    upstream: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProvisionRequest''','''    #[serde(default)]
    upstream_remote: Option<String>,
    #[serde(default)]
    upstream_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProvisionRequest'''),
('''    let upstream = req
        .source
        .upstream
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            "provision request is missing the upstream/base reference to create the branch from"
                .to_string()
        })?;''','''    let upstream = req
        .source
        .upstream_branch
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            "provision request is missing the upstream branch to create the branch from"
                .to_string()
        })?;'''),
('''/// Normalize a resolved upstream reference into the branch name the fresh
/// remote clone's `origin` remote should fetch/track. The daemon's own
/// `resolve_upstream` can hand back either a bare branch name (`"main"`) or
/// an already-`<remote>/<branch>` form (`"origin/main"`) naming *its own*
/// local checkout's remote alias -- which is not necessarily called
/// `origin` there, but this provider's fresh clone always names its remote
/// `origin` (`git clone --origin origin`), so only the branch half of a
/// slash-qualified upstream is meaningful here.
fn upstream_branch_name(upstream: &str) -> &str {
    match upstream.split_once('/') {
        Some((_, branch)) if !branch.is_empty() => branch,
        _ => upstream,
    }
}

''',''),
('''shell_quote_single(&format!("origin/{}", upstream_branch_name(args.upstream)));''','''shell_quote_single(&format!("origin/{}", args.upstream));'''),
])
