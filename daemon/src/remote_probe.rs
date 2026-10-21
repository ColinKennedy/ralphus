//! A one-call snapshot of a repository's refs on a remote machine.
//!
//! Every git command a review runs on a machine is its own Machine API call
//! (an SSH session and a runner process for the built-in providers), so an
//! idle review's background maintenance -- a dozen read-only commands every
//! few seconds -- kept its machine busy for nothing. A probe answers "did
//! anything that maintenance acts on move?" in one `run` call instead: the
//! repository's local refs, its worktrees and config, and the remote tips of
//! every branch the repository has been fetching ([`note_fetch`]).
//!
//! The probe is one authored command line run through the provider's default
//! shell -- `sh` on POSIX machines, `cmd` on Windows ones -- so it uses only
//! what both read the same way: `&&`, `echo`, and unquoted arguments. Any
//! remote or ref name outside [`is_plain_token`] makes the probe refuse
//! ([`command`] returns `None`) rather than quote it, and its caller falls
//! back to running maintenance in full.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash as _, Hasher as _};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// The line separating the probe's sections in its output.
const SECTION: &str = "ralphus-probe-section";

/// Local ref namespaces the probe lists: branches, remote-tracking refs and
/// ralphus' own (PR sync refs, live refs).
const LOCAL_REF_NAMESPACES: [&str; 3] = ["refs/heads", "refs/remotes", "refs/ralphus"];

/// Most fetch sources remembered per repository; the oldest is dropped first.
const MAX_SOURCES: usize = 64;

/// The ref a fetch reads on a remote: `src` is a full ref name, or empty for
/// a fetch that names no refspec (it reads every branch).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FetchSource {
    /// The remote the fetch names.
    pub remote: String,
    /// The full ref fetched there, or empty for every branch.
    pub src: String,
}

/// A repository on a machine: `(machine, repository path on the machine)`.
type RepoKey = (String, String);

/// Fetch sources per repository, in the order they were first fetched.
static SOURCES: LazyLock<Mutex<HashMap<RepoKey, Vec<FetchSource>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The last successful probe per repository, and when it was taken.
static LAST_PROBE: LazyLock<Mutex<HashMap<RepoKey, (Instant, RefProbe)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `ref` as a full ref name: a bare branch name becomes `refs/heads/<name>`.
fn full_ref(name: &str) -> String {
    if name.starts_with("refs/") || name == "HEAD" {
        name.to_string()
    } else {
        format!("refs/heads/{name}")
    }
}

/// The sources a `git fetch` with `args` (starting with `fetch`) reads.
fn fetch_sources(args: &[&str]) -> Vec<FetchSource> {
    let mut positional = args.iter().skip(1).filter(|a| !a.starts_with('-')).copied();
    let Some(remote) = positional.next() else {
        return Vec::new();
    };
    let specs: Vec<&str> = positional.collect();
    if specs.is_empty() {
        return vec![FetchSource {
            remote: remote.to_string(),
            src: String::new(),
        }];
    }
    specs
        .into_iter()
        .map(|spec| {
            let spec = spec.trim_start_matches('+');
            let src = spec.split_once(':').map_or(spec, |(src, _)| src);
            FetchSource {
                remote: remote.to_string(),
                src: if src.is_empty() || src.contains('*') {
                    String::new()
                } else {
                    full_ref(src)
                },
            }
        })
        .collect()
}

/// Remember what a `git fetch` (`args`, starting with `fetch`) run in `root`
/// on `machine` reads, so later probes of that repository check those
/// remote tips too.
pub fn note_fetch(machine: &str, root: &str, args: &[&str]) {
    let found = fetch_sources(args);
    if found.is_empty() {
        return;
    }
    let mut all = SOURCES.lock().expect("poisoned");
    let known = all
        .entry((machine.to_string(), root.to_string()))
        .or_default();
    for source in found {
        if !known.contains(&source) {
            known.push(source);
        }
    }
    let excess = known.len().saturating_sub(MAX_SOURCES);
    known.drain(..excess);
}

/// The fetch sources remembered for `root` on `machine`.
#[must_use]
pub fn sources(machine: &str, root: &str) -> Vec<FetchSource> {
    SOURCES
        .lock()
        .expect("poisoned")
        .get(&(machine.to_string(), root.to_string()))
        .cloned()
        .unwrap_or_default()
}

/// Forget the fetch sources of `root` on `machine` -- after a probe failed,
/// so a remote that is gone does not keep failing every later probe. The
/// next full maintenance pass remembers the ones still fetched.
pub fn forget_sources(machine: &str, root: &str) {
    SOURCES
        .lock()
        .expect("poisoned")
        .remove(&(machine.to_string(), root.to_string()));
}

/// Whether `token` can go into the probe's command line unquoted and mean
/// the same thing to `sh` and to `cmd`.
fn is_plain_token(token: &str) -> bool {
    !token.is_empty()
        && !token.starts_with('-')
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

/// `sources` grouped by remote, in remote-name order: the order the probe
/// lists them in, one `ls-remote` section each.
fn by_remote(sources: &[FetchSource]) -> BTreeMap<&str, Vec<&str>> {
    let mut grouped: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for source in sources {
        grouped
            .entry(source.remote.as_str())
            .or_default()
            .push(source.src.as_str());
    }
    grouped
}

/// The probe's command line for a repository that fetches `sources`, or
/// `None` when one of them cannot be written into it unquoted.
#[must_use]
pub fn command(sources: &[FetchSource]) -> Option<String> {
    let mut parts = vec![
        format!("git for-each-ref {}", LOCAL_REF_NAMESPACES.join(" ")),
        "git worktree list --porcelain".to_string(),
        "git config --list".to_string(),
    ];
    for (remote, srcs) in by_remote(sources) {
        if !is_plain_token(remote) {
            return None;
        }
        if srcs.iter().any(|src| src.is_empty()) {
            parts.push(format!("git ls-remote --heads {remote}"));
            continue;
        }
        if !srcs.iter().all(|src| is_plain_token(src)) {
            return None;
        }
        parts.push(format!("git ls-remote {remote} {}", srcs.join(" ")));
    }
    Some(parts.join(&format!(" && echo {SECTION} && ")))
}

/// One probe's result: a digest of everything it printed, and the ref tips
/// read from it.
#[derive(Debug, Clone)]
pub struct RefProbe {
    /// A hash of the whole output: equal digests mean nothing the probe
    /// covers moved.
    pub digest: u64,
    /// Local ref name to sha.
    local: HashMap<String, String>,
    /// `(remote, ref name)` to sha on that remote.
    remote: HashMap<(String, String), String>,
}

impl RefProbe {
    /// The sha `name` (a full ref name) points at in the repository.
    #[must_use]
    pub fn local_tip(&self, name: &str) -> Option<&str> {
        self.local.get(name).map(String::as_str)
    }

    /// The sha `name` (a branch name or full ref name) points at on `remote`.
    #[must_use]
    pub fn remote_tip(&self, remote: &str, name: &str) -> Option<&str> {
        self.remote
            .get(&(remote.to_string(), full_ref(name)))
            .map(String::as_str)
    }

    /// The sha `name` (a branch name or full ref name) points at on whichever
    /// probed remote has it, when exactly one sha is reported for it.
    #[must_use]
    pub fn any_remote_tip(&self, name: &str) -> Option<&str> {
        let wanted = full_ref(name);
        let mut tips = self
            .remote
            .iter()
            .filter(|((_, r), _)| *r == wanted)
            .map(|(_, sha)| sha.as_str());
        let first = tips.next()?;
        tips.all(|sha| sha == first).then_some(first)
    }
}

/// The leading sha of a ref-listing line, when it has one.
fn sha_field(field: &str) -> Option<&str> {
    let field = field.trim();
    (field.len() >= 40 && field.chars().all(|c| c.is_ascii_hexdigit())).then_some(field)
}

/// Read a probe's `output`, run for `sources`. `None` when it does not have
/// the sections [`command`] asked for.
#[must_use]
pub fn parse(output: &str, sources: &[FetchSource]) -> Option<RefProbe> {
    let remotes: Vec<&str> = by_remote(sources).into_keys().collect();
    let mut sections: Vec<Vec<&str>> = vec![Vec::new()];
    for line in output.lines() {
        if line.trim() == SECTION {
            sections.push(Vec::new());
        } else if let Some(section) = sections.last_mut() {
            section.push(line);
        }
    }
    if sections.len() != 3 + remotes.len() {
        return None;
    }
    let mut local = HashMap::new();
    for line in &sections[0] {
        // `<sha> <type>\t<ref>`
        if let Some((head, name)) = line.split_once('\t') {
            if let Some(sha) = head.split_whitespace().next().and_then(sha_field) {
                local.insert(name.trim().to_string(), sha.to_string());
            }
        }
    }
    let mut remote = HashMap::new();
    for (remote_name, section) in remotes.iter().zip(&sections[3..]) {
        for line in section {
            // `<sha>\t<ref>`
            if let Some((sha, name)) = line.split_once('\t') {
                if let Some(sha) = sha_field(sha) {
                    remote.insert(
                        ((*remote_name).to_string(), name.trim().to_string()),
                        sha.to_string(),
                    );
                }
            }
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    output.hash(&mut hasher);
    Some(RefProbe {
        digest: hasher.finish(),
        local,
        remote,
    })
}

/// Keep `probe` as the latest one of `root` on `machine`.
pub fn remember(machine: &str, root: &str, probe: &RefProbe) {
    LAST_PROBE.lock().expect("poisoned").insert(
        (machine.to_string(), root.to_string()),
        (Instant::now(), probe.clone()),
    );
}

/// The latest probe of `root` on `machine`, when it is younger than
/// `max_age`.
#[must_use]
pub fn recent(machine: &str, root: &str, max_age: Duration) -> Option<RefProbe> {
    let mut last = LAST_PROBE.lock().expect("poisoned");
    let key = (machine.to_string(), root.to_string());
    match last.get(&key) {
        Some((at, probe)) if at.elapsed() < max_age => Some(probe.clone()),
        Some(_) => {
            last.remove(&key);
            None
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn a_fetch_is_remembered_as_the_full_refs_it_reads() {
        assert_eq!(
            fetch_sources(&[
                "fetch",
                "--no-write-fetch-head",
                "origin",
                "+feature/a:refs/ralphus/sync/pr-1",
                "+refs/heads/main:refs/heads/main",
            ]),
            vec![
                FetchSource {
                    remote: "origin".into(),
                    src: "refs/heads/feature/a".into()
                },
                FetchSource {
                    remote: "origin".into(),
                    src: "refs/heads/main".into()
                },
            ]
        );
        assert_eq!(
            fetch_sources(&["fetch", "fork"]),
            vec![FetchSource {
                remote: "fork".into(),
                src: String::new()
            }]
        );
        assert!(fetch_sources(&["fetch", "--all"]).is_empty());
    }

    #[test]
    fn the_command_lists_each_remote_once_and_refuses_what_it_cannot_write_plainly() {
        let sources = [
            FetchSource {
                remote: "origin".into(),
                src: "refs/heads/main".into(),
            },
            FetchSource {
                remote: "fork".into(),
                src: String::new(),
            },
            FetchSource {
                remote: "origin".into(),
                src: "refs/heads/feature/a".into(),
            },
        ];
        assert_eq!(
            command(&sources).unwrap(),
            format!(
                "git for-each-ref refs/heads refs/remotes refs/ralphus && echo {SECTION} && \
                 git worktree list --porcelain && echo {SECTION} && git config --list && \
                 echo {SECTION} && git ls-remote --heads fork && echo {SECTION} && \
                 git ls-remote origin refs/heads/main refs/heads/feature/a"
            )
        );
        let odd = [FetchSource {
            remote: "origin".into(),
            src: "refs/heads/a&b".into(),
        }];
        assert_eq!(command(&odd), None);
    }

    #[test]
    fn a_probe_reads_local_and_remote_tips_and_digests_its_whole_output() {
        let sources = [FetchSource {
            remote: "origin".into(),
            src: "refs/heads/main".into(),
        }];
        // `cmd` echoes the separator with a trailing space.
        let output = format!(
            "{A} commit\trefs/heads/main\n{SECTION} \nworktree /srv/repo\n{SECTION}\n\
             core.bare=false\n{SECTION}\n{B}\trefs/heads/main\n"
        );
        let probe = parse(&output, &sources).unwrap();
        assert_eq!(probe.local_tip("refs/heads/main"), Some(A));
        assert_eq!(probe.remote_tip("origin", "main"), Some(B));
        assert_eq!(probe.any_remote_tip("main"), Some(B));
        let moved = parse(&output.replace(B, A), &sources).unwrap();
        assert_ne!(probe.digest, moved.digest);
        assert!(parse("no sections at all", &sources).is_none());
    }
}
