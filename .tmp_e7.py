import re
p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
def cut(s,a,b,new=''):
    i=s.index(a); j=s.index(b,i)
    return s[:i]+new+s[j:]
s=s.replace("    NewFromRemote { remote_ref: String },\n","")
s=cut(s,"/// Whether `branch` names an existing remote-tracking ref","/// How many `<base>-2`")
s=s.replace('''    if names_remote_tracking_branch(Path::new(&project.path), base_branch) {
        return Ok(base_branch.to_string());
    }
''','')
s=cut(s,"/// **A remote-tracking branch is exempt and always shared.**","fn resolve_squad_branch(\n","")
s=s.replace("/// included, so a branch already checked out in a hand-made worktree falls\n/// through to the next free `-2`, `-3`, ... suffix here instead of git\n/// itself rejecting the later `git worktree add`.\n///\n","/// included, so a branch already checked out in a hand-made worktree falls\n/// through to the next free `-2`, `-3`, ... suffix here instead of git\n/// itself rejecting the later `git worktree add`.\nfn resolve_squad_branch(\n") if False else s
# branch_materialization
i=s.index("    if branch.contains('/') {\n        let remote_ref")
j=s.index("    Ok(BranchMaterialization::NewFromHead { warning })\n",i)+len("    Ok(BranchMaterialization::NewFromHead { warning })\n")
s=s[:i]+"    Ok(BranchMaterialization::NewFromHead)\n"+s[j:]
s=s.replace("    NewFromHead { warning: Option<String> },","    NewFromHead,")
# execute plan arms
i=s.index("        BranchMaterialization::NewFromRemote { remote_ref } => {")
j=s.index("        BranchMaterialization::NewFromHead { warning } => {",i)
s=s[:i]+s[j:]
s=s.replace('''        BranchMaterialization::NewFromHead { warning } => {
            if let Some(warning) = warning {
                // ralphus[ignore-rlog-pair]: this low-level helper has no Store; its Store-owning caller records the structured workflow outcome
                crate::rlog!(WARNING, "ralphus [scheduler] {warning}");
            }
''','''        BranchMaterialization::NewFromHead => {
''')
s=s.replace("let qualified_upstream = qualify_ambiguous_ref(root, upstream);","let qualified_upstream = qualify_upstream_ref(root, upstream);")
s=s.replace("// `--no-track`: see the `NewFromRemote` arm above -- `upstream`\n            // being a plain local branch still triggers","// `--no-track`: `upstream` being a plain local branch still triggers")
s=s.replace('''    root: &Path,
    branch: &str,
    upstream: &str,
    plan: &WorktreePlan,''','''    root: &Path,
    branch: &str,
    upstream: &UpstreamRef,
    plan: &WorktreePlan,''')
s=s.replace('''pub(crate) fn ensure_worktree_tracked(
    root: &Path,
    branch: &str,
    upstream: &str,''','''pub(crate) fn ensure_worktree_tracked(
    root: &Path,
    branch: &str,
    upstream: &UpstreamRef,''')
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
