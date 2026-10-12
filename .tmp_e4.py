import re
def edit(path, pairs):
    s = open(path, encoding='utf-8', newline='').read()
    crlf = '\r\n' in s
    s = s.replace('\r\n', '\n')
    for a, b in pairs:
        assert s.count(a) == 1, (path, a[:60], s.count(a))
        s = s.replace(a, b)
    if crlf: s = s.replace('\n', '\r\n')
    open(path, 'w', encoding='utf-8', newline='').write(s)

edit('daemon/src/guardian_merge.rs', [
("""    pub status: String,
    pub base_branch: String,
    pub projects: Vec<String>,""", """    pub status: String,
    pub base_branch: UpstreamRef,
    pub projects: Vec<String>,"""),
("""    pub machine: Option<String>,
    pub base_branch: String,
}""", """    pub machine: Option<String>,
    pub base_branch: UpstreamRef,
}"""),
("""    let base = &target.base_branch;

    if let Some((remote, branch)) = base.split_once('/') {
        if ws.git(&["remote", "get-url", remote]).is_ok() {
            let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
            return ws
                .git(&["fetch", "--no-write-fetch-head", remote, &refspec])
                .map(|_| ());
        }
    }
    if let Ok(upstream) = ws.git(&["rev-parse", "--abbrev-ref", &format!("{base}@{{upstream}}")]) {
        let upstream = upstream.trim();
        if let Some((remote, branch)) = upstream.split_once('/') {
            let refspec = format!("+refs/heads/{branch}:refs/heads/{base}");
            return ws
                .git(&["fetch", "--no-write-fetch-head", remote, &refspec])
                .map(|_| ());
        }
    }
    Ok(())
""", """    let base = &target.base_branch;

    if !base.is_local() {
        let (remote, branch) = (&base.remote, &base.branch);
        if ws.git(&["remote", "get-url", remote]).is_ok() {
            let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
            return ws
                .git(&["fetch", "--no-write-fetch-head", remote, &refspec])
                .map(|_| ());
        }
        return Ok(());
    }
    let branch = &base.branch;
    let tracked = |key: &str| {
        ws.git(&["config", "--get", &format!("branch.{branch}.{key}")])
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if let (Some(remote), Some(merge)) = (tracked("remote"), tracked("merge")) {
        if remote != ralphus_core::schema::LOCAL_UPSTREAM_REMOTE {
            let upstream_branch = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
            let refspec = format!("+refs/heads/{upstream_branch}:refs/heads/{branch}");
            return ws
                .git(&["fetch", "--no-write-fetch-head", &remote, &refspec])
                .map(|_| ());
        }
    }
    Ok(())
"""),
("""            target.machine.as_deref().unwrap_or(""),
            target.base_branch
        );""", """            target.machine.as_deref().unwrap_or(""),
            target.base_branch.joined()
        );"""),
("""                        "ralphus [guardian] base-branch freshness fetch skipped for {} ({}): {e}",
                        target.base_branch,""", """                        "ralphus [guardian] base-branch freshness fetch skipped for {} ({}): {e}",
                        target.base_branch.joined(),"""),
("""                            "base_branch": target.base_branch,
                            "git_root": target.root,""", """                            "base_branch": target.base_branch.joined(),
                            "git_root": target.root,"""),
("""                &info.base_branch,
            );
        });
    }
}""", """                &info.base_branch,
            );
        });
    }
}"""),
("""                Some(crate::forge::resolve_parent_remote_name(
                    Path::new(&branch_project),
                    &base,""", """                Some(crate::forge::resolve_parent_remote_name(
                    Path::new(&branch_project),
                    &base,"""),
("""    let base_branch = review
        .as_ref()
        .map(|g| g.base_branch.as_str())
        .unwrap_or_default();""", """    let base_branch = review
        .as_ref()
        .map(GuardianView::upstream)
        .unwrap_or_else(|| UpstreamRef::local(""));"""),
("""        root,
        base_branch,
        &crate::config::resolve_forge(root),""", """        root,
        &base_branch,
        &crate::config::resolve_forge(root),"""),
("""    fn fetch_info(
        status: &str,
        base_branch: &str,""", """    fn fetch_info(
        status: &str,
        base_branch: &str,"""),
("""            base_branch: base_branch.to_string(),
            projects: projects.iter()""", """            base_branch: split_test_ref(base_branch),
            projects: projects.iter()"""),
])
edit('daemon/src/guardian.rs', [
("""            "SELECT g.status, g.base_branch, g.git_root, g.machine,""", """            "SELECT g.status, g.upstream_remote, g.upstream_branch, g.git_root, g.machine,"""),
])
