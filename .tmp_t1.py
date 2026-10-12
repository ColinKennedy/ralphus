p='daemon/src/triage.rs'
s=open(p,encoding='utf-8').read()
def rep(a,b):
    global s
    assert s.count(a)==1,a
    s=s.replace(a,b)
rep("    pub branch: String,\n    pub upstream: String,\n}","    pub branch: String,\n    pub upstream: UpstreamRef,\n}")
rep("        branch: &str,\n        upstream: &str,\n    ) -> StoreResult<()> {\n        self.conn.execute(\n            \"INSERT INTO triage_pool_cells(project, triage_type, squad_id, task_idx, idx, branch, upstream, created_at_ms)\n             VALUES(?,?,?,?,?,?,?,?)\",\n            params![project, triage_type, squad_id, task_idx, idx, branch, upstream, now_ms()],",
"""        branch: &str,
        upstream: &UpstreamRef,
    ) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO triage_pool_cells(project, triage_type, squad_id, task_idx, idx, branch, upstream, upstream_remote, upstream_branch, created_at_ms)
             VALUES(?,?,?,?,?,?,?,?,?,?)",
            params![
                project,
                triage_type,
                squad_id,
                task_idx,
                idx,
                branch,
                upstream.joined(),
                upstream.remote,
                upstream.branch,
                now_ms()
            ],""")
rep('"SELECT project, triage_type, squad_id, task_idx, idx, branch, upstream FROM triage_pool_cells"','"SELECT project, triage_type, squad_id, task_idx, idx, branch, upstream_remote, upstream_branch FROM triage_pool_cells"')
rep("                        upstream: r.get(6)?,","                        upstream: UpstreamRef::new(\n                            r.get::<_, String>(6)?,\n                            r.get::<_, String>(7)?,\n                        ),")
rep('"SELECT squad_id, task_idx, idx, branch, upstream FROM triage_pool_cells','"SELECT squad_id, task_idx, idx, branch, upstream_remote, upstream_branch FROM triage_pool_cells')
rep("                    upstream: r.get(4)?,","                    upstream: UpstreamRef::new(r.get::<_, String>(4)?, r.get::<_, String>(5)?),")
open(p,'w',encoding='utf-8').write(s)
