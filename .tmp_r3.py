p='daemon/src/reviews.rs'
s=open(p,encoding='utf-8').read()
a="""pub(crate) fn rebase_onto(cwd: &Path, target_branch: &str) -> std::result::Result<(), String> {
"""
b="""pub(crate) fn rebase_onto(
    cwd: &Path,
    target: impl Into<UpstreamRef>,
) -> std::result::Result<(), String> {
    let target_branch = qualify_upstream_ref(cwd, &target.into());
    let target_branch = target_branch.as_str();
"""
assert a in s
s=s.replace(a,b)
open(p,'w',encoding='utf-8').write(s)
