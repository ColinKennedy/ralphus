p='daemon/src/worktrees.rs'
s=open(p,encoding='utf-8',newline='').read()
crlf='\r\n' in s
s=s.replace('\r\n','\n')
a=s.index('/// Resolve a placeholder cell `cwd`\'s `?upstream=` value')
b=s.index('/// For a project registered with a remote (`clone_url` set via')
new='''/// Resolve a placeholder cell `cwd`'s explicit upstream (`upstream_remote` +
/// `upstream_branch`, RAL-628) against the project `root`, returning the
/// pair with its branch half concrete.
///
/// `<<default>>` (branch half only) becomes the default branch of the *named*
/// remote -- the branch that remote's `HEAD` symref points at -- and
/// `<<current_branch>>` becomes whatever branch `root` currently has checked
/// out (valid only with the local remote `.`). Lookup failures are hard
/// errors: ralphus never guesses `main`/`master`. Any other `<<...>>` value
/// (a typo, or an unsupported sentinel) is rejected here, defense-in-depth on
/// top of the submit-time check in `ralphus_core::validate`.
fn resolve_upstream(
    root: &Path,
    upstream: ralphus_core::schema::WorktreeUpstream<'_>,
) -> Result<ResolvedUpstream, String> {
    use ralphus_core::schema::{
        LOCAL_UPSTREAM_REMOTE, WORKTREE_UPSTREAM_CURRENT_BRANCH, WORKTREE_UPSTREAM_DEFAULT,
    };
    let remote = upstream.remote;
    let branch = match upstream.branch {
        WORKTREE_UPSTREAM_DEFAULT => default_branch(root, remote)?,
        WORKTREE_UPSTREAM_CURRENT_BRANCH => {
            if remote != LOCAL_UPSTREAM_REMOTE {
                return Err(format!(
                    "\\"upstream_branch={WORKTREE_UPSTREAM_CURRENT_BRANCH}\\" names the branch \
                     checked out in the project root, so it requires \
                     \\"upstream_remote={LOCAL_UPSTREAM_REMOTE}\\" (got \\"{remote}\\")"
                ));
            }
            current_checked_out_branch(root)?
        }
        other if other.starts_with("<<") => {
            return Err(format!(
                "\\"upstream_branch={other}\\" is not a supported sentinel; use \
                 \\"{WORKTREE_UPSTREAM_DEFAULT}\\" or \\"{WORKTREE_UPSTREAM_CURRENT_BRANCH}\\", \
                 or a literal branch name"
            ));
        }
        other => other.to_string(),
    };
    Ok(ResolvedUpstream {
        remote: remote.to_string(),
        branch,
    })
}

/// A worktree placeholder's upstream after sentinel expansion: a remote (or
/// `.` for a local branch) and a literal branch on it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ResolvedUpstream {
    pub(crate) remote: String,
    pub(crate) branch: String,
}

impl ResolvedUpstream {
    /// The joined `<remote>/<branch>` display string (just `<branch>` for a
    /// local base), the form the worktree materialization helpers key on.
    pub(crate) fn joined(&self) -> String {
        ralphus_core::schema::join_upstream(&self.remote, &self.branch)
    }
}

'''
s=s[:a]+new+s[b:]
# default_branch
a=s.index('fn default_branch(root: &Path) -> Result<String, String> {')
b=s.index('/// The branch currently checked out in the project\'s registered root/primary')
newdb='''fn default_branch(root: &Path, remote: &str) -> Result<String, String> {
    if remote == ralphus_core::schema::LOCAL_UPSTREAM_REMOTE {
        return Err(
            "\\"upstream_branch=<<default>>\\" needs a named remote to ask for its default \
             branch; \\"upstream_remote=.\\" is a local base"
                .to_string(),
        );
    }
    let prefix = format!("refs/remotes/{remote}/");
    let symref = format!("{prefix}HEAD");
    if let Ok(out) = git(root, &["symbolic-ref", &symref]) {
        if let Some(branch) = out.trim().strip_prefix(prefix.as_str()) {
            if !branch.is_empty() {
                return Ok(branch.to_string());
            }
        }
    }
    Err(format!(
        "\\"upstream_branch=<<default>>\\" could not resolve a default branch for remote \
         \\"{remote}\\" in {}: `refs/remotes/{remote}/HEAD` is not set. Fix: run \
         `git remote set-head {remote} --auto` in this repository, or use a literal branch \
         name in upstream_branch instead",
        root.display()
    ))
}

'''
s=s[:a]+newdb+s[b:]
if crlf: s=s.replace('\n','\r\n')
open(p,'w',encoding='utf-8',newline='').write(s)
