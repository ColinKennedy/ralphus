p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
a="///\n/// Also the sole gate for the remote-tracking exemption"
i=s.index(a); j=s.index("fn resolve_squad_branch_claim(",i)
s=s[:i]+s[j:]
s=s.replace("/// through to the next free `-2`, `-3`, ... suffix here instead of git\n/// itself rejecting the later `git worktree add`.\n///\nfn resolve_squad_branch(","/// through to the next free `-2`, `-3`, ... suffix here instead of git\n/// itself rejecting the later `git worktree add`.\nfn resolve_squad_branch(")
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
