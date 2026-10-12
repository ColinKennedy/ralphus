import re
p='daemon/src/pr.rs'
s=open(p,encoding='utf-8').read()
i=s.index("/// The remote name to strip/qualify a guardian's `base_branch`")
j=s.index("/// Resolve [`PrRepoRouting`] for `guardian`'s project")
s=s[:i]+s[j:]
s=s.replace("""        let forge_cfg = crate::config::resolve_forge(&root_dir);
        let remote_name = forge_parent_remote_name(&store, &root_dir, "alt/main", &forge_cfg, None);
        let local_base = qualify_forge_base("alt/main", &remote_name, &drift.base);
""","""        let local_base = UpstreamRef::new("alt", &drift.base);
""")
open(p,'w',encoding='utf-8').write(s)
