p='daemon/src/followup.rs'
s=open(p,encoding='utf-8').read()
def rep(a,b):
    global s
    assert s.count(a)==1,a
    s=s.replace(a,b)
rep("use crate::guardian::GuardianView;","use ralphus_core::schema::UpstreamRef;\n\nuse crate::guardian::GuardianView;")
rep("""    base: &str,
    items: &[FollowupItem],
) -> String {""","""    base: &UpstreamRef,
    items: &[FollowupItem],
) -> String {""")
rep('format!("<<ralphus:new-worktree/{branch}?upstream={base}>>").into(),','''format!(
                    "<<ralphus:new-worktree/{branch}?upstream_remote={}&upstream_branch={}>>",
                    base.remote, base.branch
                )
                .into(),''')
i=s.index("/// Splits a stored merge target into")
j=s.index("/// What accepting an offer created.")
new='''/// Whether `base` exists on its remote, falling back to the local refs when
/// the remote cannot be asked (no such remote, offline). A local base is
/// looked up in the local refs only.
fn branch_exists(root: &Path, base: &UpstreamRef) -> bool {
    let git = |args: &[&str]| {
        ralphus_core::git_spawn::command(args)
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
    };
    let heads = format!("refs/heads/{}", base.branch);
    if !base.is_local() {
        if let Ok(out) = git(&["ls-remote", "--exit-code", "--heads", &base.remote, &heads]) {
            match out.status.code() {
                Some(0) => return true,
                // `--exit-code`: 2 means the remote answered and has no such ref.
                Some(2) => return false,
                _ => {}
            }
        }
    }
    git(&["rev-parse", "--verify", "--quiet", &base.tracking_ref()])
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The upstream a follow-up squad is based on: the branch the review merged
/// into, or the default branch of the same remote (`origin` for a local base)
/// when that branch is gone.
#[must_use]
pub fn resolve_followup_base(git_root: &Path, merged_into: &UpstreamRef) -> UpstreamRef {
    if !merged_into.branch.is_empty() && branch_exists(git_root, merged_into) {
        merged_into.clone()
    } else {
        let remote = if merged_into.is_local() {
            "origin"
        } else {
            merged_into.remote.as_str()
        };
        UpstreamRef::new(remote, DEFAULT_UPSTREAM)
    }
}

'''
s=s[:i]+new+s[j:]
rep("let base = resolve_followup_base(root, &guardian.base_branch);","let base = resolve_followup_base(root, &guardian.upstream());")
rep(") -> std::result::Result<(String, String, bool), AcceptError> {",") -> std::result::Result<(String, UpstreamRef, bool), AcceptError> {")
rep("    Ok(AcceptResult {\n        squad_id,\n        base,","    Ok(AcceptResult {\n        squad_id,\n        base: base.joined(),")
open(p,'w',encoding='utf-8').write(s)

p='daemon/src/ark.rs'
s=open(p,encoding='utf-8').read()
a=s.index('    let upstream = git(\n        worktree,\n        &["rev-parse", "--symbolic-full-name"')
b=s.index('    let remote_output = git(')
s=s[:a]+'''    let upstream = crate::reviews::worktree_upstream(worktree)?;
    if upstream.is_local() {
        return Err(format!("upstream {upstream} is not a remote-tracking ref"));
    }
    let UpstreamRef { remote, branch } = upstream;
'''+s[b:]
s=s.replace("            remote,\n            &format!(\"refs/heads/{branch}\"),","            &remote,\n            &format!(\"refs/heads/{branch}\"),")
open(p,'w',encoding='utf-8').write(s)
