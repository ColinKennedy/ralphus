p='daemon/src/reviews.rs'
s=open(p,encoding='utf-8').read()
def rep(a,b,c=1):
    global s
    assert s.count(a)==c,(a,s.count(a))
    s=s.replace(a,b)
rep("""    match pending {
        Some(p) => Some(p.upstream),
        None => worktree_upstream(Path::new(cwd)).ok(),
    }""","""    match pending {
        Some(p) => Some(p.upstream.joined()),
        None => worktree_upstream(Path::new(cwd)).ok().map(|u| u.joined()),
    }""")
rep("    upstream: String,\n    name: String,\n    order: usize,","    upstream: UpstreamRef,\n    name: String,\n    order: usize,")
rep(""") -> Vec<(ProjectView, String)> {
    let (cells, tasks, cell_info) = rows_from_file(file);
    let mut seen: HashSet<(String, String)> = HashSet::new();""",""") -> Vec<(ProjectView, UpstreamRef)> {
    let (cells, tasks, cell_info) = rows_from_file(file);
    let mut seen: HashSet<(String, UpstreamRef)> = HashSet::new();""")
rep("""        let Some(upstream) = review_map
            .get(rev_id)
            .copied()
            .and_then(|r| r.upstream.as_deref())
            .map(str::trim)
            .filter(|b| !b.is_empty())
        else {
            continue;
        };
        // `ReviewDef.upstream` never supports a `<<...>>` sentinel (unlike a
        // cell's own `?upstream=`) -- only the already-explicit-remote form
        // (`contains('/')`) needs skipping here, matching
        // `resolve_registered_remote_upstream`'s own no-op check.
        if upstream.contains('/') {
            continue;
        }
""","""        let Some(upstream) = review_map.get(rev_id).copied().and_then(declared_upstream)
        else {
            continue;
        };
        // A local base is never fetched.
        if upstream.is_local() {
            continue;
        }
""")
rep("if !seen.insert((project_name.to_string(), upstream.to_string())) {","if !seen.insert((project_name.to_string(), upstream.clone())) {")
rep("        out.push((project, upstream.to_string()));\n    }\n    out\n}","        out.push((project, upstream));\n    }\n    out\n}")
rep("""        let declared_upstream = review_map
            .get(rev_id)
            .copied()
            .and_then(|r| r.upstream.clone())
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());""","""        let declared_upstream = review_map.get(rev_id).copied().and_then(declared_upstream);""")
rep("""on the review: [[review]] upstream = \\"main\\".",""","""on the review: [[review]] upstream_remote = \\".\\" and upstream_branch = \\"main\\".",""")
rep("""prefetched_upstreams.get(&(pv.name.clone(), b.clone())) {""","""prefetched_upstreams.get(&(pv.name.clone(), b.clone())) {""")
rep("""as [[review]] upstream = \\"<branch>\\")\"""","""as [[review]] upstream_remote/upstream_branch)\"""")
rep("""/// Every `(registered project, bare upstream)` pair""","""/// The upstream a `[[review]]` block declares, `None` unless both halves of
/// the pair are present and non-empty.
fn declared_upstream(review: &ReviewDef) -> Option<UpstreamRef> {
    let remote = review.upstream_remote.as_deref()?.trim();
    let branch = review.upstream_branch.as_deref()?.trim();
    (!remote.is_empty() && !branch.is_empty()).then(|| UpstreamRef::new(remote, branch))
}

/// Every `(registered project, upstream)` pair""")
open(p,'w',encoding='utf-8').write(s)
