//! `base_update_mode = "required"`: decide whether a base shift needs the review
//! stack rebuilt, from a local preflight and the forge's mergeability verdict.
//!
//! The decision is read-only. The local preflight is a `git merge-tree
//! --write-tree` of each ordered review branch (the last one is the combined
//! stack tip) against the shifted upstream SHA; it writes unreferenced tree
//! objects only and never touches a ref, index or worktree, so there is no
//! scratch state to clean up. The forge half asks the existing REST clients
//! for the mergeability of the open PRs/MRs filed against the base branch --
//! stacked PRs target the previous branch, not the base, and say nothing about
//! the shift. Every SHA the verdict depends on is recorded, and the verdict is
//! discarded when any of them moved while it was computed.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use crate::forge::{ForgeClient, PrRebaseNeed};
use crate::guardian::{BranchView, GuardianView};
use crate::workspace::Workspace;

/// How many forge reads a still-computing verdict gets before it is inconclusive.
const FORGE_ATTEMPTS: u32 = 4;
/// Pause between forge reads of a still-computing verdict.
const FORGE_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Consecutive inconclusive assessments of one unchanged stack before the
/// review's watchers are told.
pub const PERSISTENT_INCONCLUSIVE_PASSES: u32 = 3;

/// Whether the stack must be rebuilt on the shifted base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing needs rebuilding.
    Clean,
    /// A conflict or an explicit rebase request was found.
    RebaseRequired,
    /// The question could not be answered safely.
    Inconclusive,
}

/// A [`Verdict`] with the evidence behind it.
#[derive(Debug, Clone)]
pub struct Assessment {
    pub verdict: Verdict,
    /// One line per finding, shown in logs, Cartographer and mailbox notices.
    pub reasons: Vec<String>,
    /// Every SHA the verdict was computed from, keyed by a stable label.
    pub shas: BTreeMap<String, String>,
    /// `true` when no forge verdict backs a clean answer (a local-only review),
    /// so it is not remembered between passes.
    pub provisional: bool,
    /// `true` when the answer was thrown away because a SHA moved meanwhile.
    pub discarded: bool,
}

impl Assessment {
    fn inconclusive(reason: String, shas: BTreeMap<String, String>) -> Self {
        Self {
            verdict: Verdict::Inconclusive,
            reasons: vec![reason],
            shas,
            provisional: false,
            discarded: false,
        }
    }

    /// Stable identity of the evidence: equal when no input SHA changed.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.shas)
    }
}

fn fingerprint(shas: &BTreeMap<String, String>) -> String {
    shas.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// What the gate remembers about one review between maintenance passes.
#[derive(Default)]
struct Remembered {
    clean_fingerprint: Option<String>,
    inconclusive_fingerprint: Option<String>,
    inconclusive_passes: u32,
    notified_fingerprint: Option<String>,
}

static REMEMBERED: Mutex<Option<BTreeMap<String, Remembered>>> = Mutex::new(None);

fn with_remembered<R>(id: &str, f: impl FnOnce(&mut Remembered) -> R) -> R {
    let mut guard = REMEMBERED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard
        .get_or_insert_with(BTreeMap::new)
        .entry(id.to_string())
        .or_default())
}

/// Forget everything remembered about review `id` (retirement cleanup).
pub fn forget(id: &str) {
    let mut guard = REMEMBERED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(map) = guard.as_mut() {
        map.remove(id);
    }
}

/// Enabled, built branches of `project`, in stack order.
fn project_branches<'a>(guardian: &'a GuardianView, project: &str) -> Vec<&'a BranchView> {
    let mut branches: Vec<&BranchView> = guardian
        .branches
        .iter()
        .filter(|b| b.enabled)
        .filter(|b| b.project.as_deref().unwrap_or(guardian.git_root.as_str()) == project)
        .filter(|b| b.review_branch.as_deref().is_some_and(|r| !r.is_empty()))
        .collect();
    branches.sort_by_key(|b| b.position);
    branches
}

/// The SHAs a verdict depends on: each shifted project's upstream target and
/// every built review branch tip in it. `Err` names a ref that did not resolve.
fn stack_shas(
    store: &crate::store_lock::StoreHandle,
    guardian: &GuardianView,
    targets: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut shas = BTreeMap::new();
    for (project, upstream) in targets {
        shas.insert(format!("{project}:upstream"), upstream.clone());
        let root = Workspace::for_guardian(store, &guardian.id, Path::new(project));
        for branch in project_branches(guardian, project) {
            let rev = branch.review_branch.as_deref().unwrap_or_default();
            let tip = root
                .git(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
                .map_err(|e| format!("{project}: review branch {rev} does not resolve: {e}"))?;
            shas.insert(format!("{project}:{rev}"), tip.trim().to_string());
        }
    }
    Ok(shas)
}

/// Whether a clean verdict for exactly this stack and base is already
/// remembered, so a maintenance pass can skip the whole assessment.
#[must_use]
pub fn known_clean(
    store: &crate::store_lock::StoreHandle,
    guardian: &GuardianView,
    targets: &BTreeMap<String, String>,
) -> bool {
    let Ok(shas) = stack_shas(store, guardian, targets) else {
        return false;
    };
    let fp = fingerprint(&shas);
    with_remembered(&guardian.id, |r| {
        r.clean_fingerprint.as_deref() == Some(fp.as_str())
    })
}

/// One local merge-tree outcome.
enum Merge {
    Clean,
    Conflict,
    Failed(String),
}

fn merge_tree(root: &Workspace, upstream: &str, rev: &str) -> Merge {
    match root.git(&["merge-tree", "--write-tree", upstream, rev]) {
        Ok(_) => Merge::Clean,
        // A conflicting merge exits non-zero with its report on stdout and an
        // empty stderr, which `Workspace::git` renders as a message ending
        // right after "failed:"; every other failure (old git, unresolvable
        // ref) carries git's own explanation.
        Err(e) if e.trim_end().ends_with(" failed:") => Merge::Conflict,
        Err(e) => Merge::Failed(e),
    }
}

/// The local half: does each ordered review branch (the last is the combined
/// tip) still merge cleanly into the shifted upstream?
fn local_preflight(
    store: &crate::store_lock::StoreHandle,
    guardian: &GuardianView,
    targets: &BTreeMap<String, String>,
    shas: BTreeMap<String, String>,
) -> Assessment {
    let mut conflicts = Vec::new();
    let mut failures = Vec::new();
    for (project, upstream) in targets {
        let branches = project_branches(guardian, project);
        let Some(last) = branches.len().checked_sub(1) else {
            failures.push(format!("{project}: no built review branch to check"));
            continue;
        };
        let root = Workspace::for_guardian(store, &guardian.id, Path::new(project));
        for (index, branch) in branches.iter().enumerate() {
            let rev = branch.review_branch.as_deref().unwrap_or_default();
            let what = if index == last {
                format!("combined tip {rev}")
            } else {
                format!("branch {rev}")
            };
            match merge_tree(&root, upstream, rev) {
                Merge::Clean => {}
                Merge::Conflict => conflicts.push(format!(
                    "{project}: {what} conflicts with {}",
                    &upstream[..upstream.len().min(8)]
                )),
                Merge::Failed(e) => failures.push(format!("{project}: {what}: {e}")),
            }
        }
    }
    let (verdict, reasons) = if !conflicts.is_empty() {
        (Verdict::RebaseRequired, conflicts)
    } else if !failures.is_empty() {
        (Verdict::Inconclusive, failures)
    } else {
        (Verdict::Clean, Vec::new())
    };
    Assessment {
        verdict,
        reasons,
        shas,
        provisional: false,
        discarded: false,
    }
}

/// One PR/MR's forge answer, after retries and validation.
#[derive(Debug, PartialEq, Eq)]
pub enum PrCheck {
    Clean,
    Required(String),
    Inconclusive(String),
}

/// Read `number`'s mergeability, retrying a still-computing answer up to
/// `attempts` times `delay` apart. The answer only counts when the forge's
/// head matches `expected_head` (when known) and its base ref matches
/// `expected_base`; a moved head or retargeted base is inconclusive.
pub fn check_pr(
    client: &ForgeClient,
    number: i64,
    expected_head: Option<&str>,
    expected_base: &str,
    attempts: u32,
    delay: Duration,
) -> PrCheck {
    let mut last = String::from("the forge never reported a verdict");
    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            std::thread::sleep(delay);
        }
        let seen = match client.pr_mergeability(number) {
            Ok(seen) => seen,
            Err(e) => {
                last = format!("PR #{number}: forge read failed: {e}");
                continue;
            }
        };
        if let (Some(want), Some(got)) = (expected_head, seen.head.as_deref()) {
            if want != got {
                return PrCheck::Inconclusive(format!(
                    "PR #{number}: forge head {} differs from the local tip {}",
                    &got[..got.len().min(8)],
                    &want[..want.len().min(8)]
                ));
            }
        }
        if let Some(base) = seen.base_ref.as_deref() {
            if base != expected_base {
                return PrCheck::Inconclusive(format!(
                    "PR #{number}: forge base {base} differs from the recorded base {expected_base}"
                ));
            }
        }
        match seen.need {
            PrRebaseNeed::Clean => return PrCheck::Clean,
            PrRebaseNeed::Required(why) => {
                return PrCheck::Required(format!("PR #{number}: {why}"));
            }
            PrRebaseNeed::Computing => {
                last = format!("PR #{number}: the forge is still computing mergeability");
            }
            PrRebaseNeed::Unknown(why) => {
                return PrCheck::Inconclusive(format!("PR #{number}: {why}"));
            }
        }
    }
    PrCheck::Inconclusive(last)
}

/// The forge half: every open PR/MR filed against the base branch.
fn forge_preflight(
    store: &crate::store_lock::StoreHandle,
    guardian: &GuardianView,
    shas: &mut BTreeMap<String, String>,
) -> (Verdict, Vec<String>, bool) {
    let prs = store
        .lock()
        .list_pull_requests_for_guardian(&guardian.id)
        .unwrap_or_default();
    let roots: Vec<_> = prs
        .into_iter()
        .filter(|pr| pr.state == "open" && pr.base_ref == guardian.base_branch)
        .filter_map(|pr| pr.pr_number.map(|n| (pr, n)))
        .collect();
    if roots.is_empty() {
        return (Verdict::Clean, Vec::new(), true);
    }
    let root = std::path::PathBuf::from(&guardian.git_root);
    let forge_cfg = crate::config::resolve_forge(&root);
    let mut required = Vec::new();
    let mut inconclusive = Vec::new();
    for (pr, number) in roots {
        let Some(client) = crate::pr::forge_client_for_pr(
            store,
            &root,
            &guardian.base_branch,
            &forge_cfg,
            &pr.repo,
            guardian.owner.as_deref(),
        ) else {
            inconclusive.push(format!("PR #{number}: no forge client for {}", pr.repo));
            continue;
        };
        let expected_head = pr
            .branch_id
            .as_deref()
            .and_then(|branch_id| guardian.branches.iter().find(|b| b.id == branch_id))
            .and_then(|b| {
                let rev = b.review_branch.as_deref()?;
                let project = b.project.as_deref().unwrap_or(guardian.git_root.as_str());
                shas.get(&format!("{project}:{rev}")).cloned()
            })
            .or_else(|| pr.last_pushed_sha.clone());
        if let Some(head) = &expected_head {
            shas.insert(format!("pr:{}:{number}", pr.repo), head.clone());
        }
        match check_pr(
            &client,
            number,
            expected_head.as_deref(),
            &guardian.base_branch,
            FORGE_ATTEMPTS,
            FORGE_RETRY_DELAY,
        ) {
            PrCheck::Clean => {}
            PrCheck::Required(why) => required.push(why),
            PrCheck::Inconclusive(why) => inconclusive.push(why),
        }
    }
    if !required.is_empty() {
        (Verdict::RebaseRequired, required, false)
    } else if !inconclusive.is_empty() {
        (Verdict::Inconclusive, inconclusive, false)
    } else {
        (Verdict::Clean, Vec::new(), false)
    }
}

/// Assess whether the shifted `targets` (project -> new upstream SHA) require
/// rebuilding `guardian`'s stack. Holds no store lock across git or the forge.
#[must_use]
pub fn assess(
    store: &crate::store_lock::StoreHandle,
    guardian: &GuardianView,
    targets: &BTreeMap<String, String>,
) -> Assessment {
    let mut shas = match stack_shas(store, guardian, targets) {
        Ok(shas) => shas,
        Err(e) => return Assessment::inconclusive(e, BTreeMap::new()),
    };
    let initial = shas.clone();
    let local = local_preflight(store, guardian, targets, shas.clone());
    // A local conflict already answers the question; skip the forge round trip.
    let mut result = if local.verdict == Verdict::RebaseRequired {
        local
    } else {
        let (forge, reasons, provisional) = forge_preflight(store, guardian, &mut shas);
        let (verdict, reasons) = match (local.verdict, forge) {
            (_, Verdict::RebaseRequired) => (Verdict::RebaseRequired, reasons),
            (Verdict::Inconclusive, _) => (Verdict::Inconclusive, local.reasons),
            (_, Verdict::Inconclusive) => (Verdict::Inconclusive, reasons),
            _ => (Verdict::Clean, Vec::new()),
        };
        Assessment {
            verdict,
            reasons,
            shas: shas.clone(),
            provisional,
            discarded: false,
        }
    };
    // Discard the answer if the stack or the base moved while it was computed.
    let moved = match stack_shas(store, guardian, targets) {
        Ok(now) => now != initial,
        Err(_) => true,
    };
    if moved {
        result = Assessment {
            discarded: true,
            ..Assessment::inconclusive(
                "the stack or the base moved while it was being assessed".to_string(),
                initial,
            )
        };
    }
    result
}

/// What the maintenance pass does with an assessment.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// Rebuild the stack.
    Rebuild,
    /// Leave the stack and its recorded built base alone.
    Skip,
    /// Leave it alone, and the same inconclusive answer has now persisted long
    /// enough that watchers must be told.
    SkipAndNotify,
}

/// Fold `assessment` into what is remembered about review `id` and decide.
pub fn record(id: &str, assessment: &Assessment) -> Gate {
    let fp = assessment.fingerprint();
    with_remembered(id, |r| match assessment.verdict {
        Verdict::RebaseRequired => {
            *r = Remembered::default();
            Gate::Rebuild
        }
        Verdict::Clean => {
            r.inconclusive_fingerprint = None;
            r.inconclusive_passes = 0;
            r.notified_fingerprint = None;
            r.clean_fingerprint = (!assessment.provisional).then_some(fp);
            Gate::Skip
        }
        Verdict::Inconclusive => {
            r.clean_fingerprint = None;
            if assessment.discarded {
                return Gate::Skip;
            }
            if r.inconclusive_fingerprint.as_deref() == Some(fp.as_str()) {
                r.inconclusive_passes += 1;
            } else {
                r.inconclusive_fingerprint = Some(fp.clone());
                r.inconclusive_passes = 1;
            }
            if r.inconclusive_passes >= PERSISTENT_INCONCLUSIVE_PASSES
                && r.notified_fingerprint.as_deref() != Some(fp.as_str())
            {
                r.notified_fingerprint = Some(fp);
                Gate::SkipAndNotify
            } else {
                Gate::Skip
            }
        }
    })
}

/// Tell the review's watchers that the base shifted but whether to rebuild
/// could not be determined, with what to do about it. Also logged to stderr
/// and Cartographer.
pub fn notify_inconclusive(
    store: &crate::store_lock::StoreHandle,
    guardian: &GuardianView,
    assessment: &Assessment,
) {
    let id = &guardian.id;
    let reasons = assessment.reasons.join("; ");
    let text = format!(
        "Review {id} ({}): its base branch {} moved, and whether the stack needs a rebase could \
         not be determined after {PERSISTENT_INCONCLUSIVE_PASSES} checks ({reasons}). The stack \
         was left untouched.",
        guardian.name, guardian.base_branch
    );
    let remediation = crate::mailbox::Remediation::ManualInterventionRequired {
        guidance: format!(
            "fix what the check reports (forge token or reachability, a missing review branch, \
             git older than 2.38), or press Merge / rebase (or run `ralphus review merge {id}`) \
             to rebuild the stack on the new base regardless"
        ),
    };
    let entity_uri = format!("guardian:{id}");
    let enqueued = {
        let guard = store.lock();
        guard.enqueue_error_mailbox_message(
            crate::mailbox::MailboxPriority::High,
            &text,
            &remediation,
            None,
            None,
            None,
            Some(&entity_uri),
            Some("review"),
        )
    };
    match enqueued {
        Ok(_) => {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {id} base-update check inconclusive: {reasons}"
            );
            crate::cartographer::Note::new("guardian")
                .level(crate::logging::LogLevel::WARNING)
                .guardian(id)
                .emit(
                    &store.lock(),
                    "base-update check inconclusive: notified mailbox",
                    serde_json::json!({"reasons": assessment.reasons, "shas": assessment.shas}),
                );
        }
        Err(e) => {
            // Let a later pass notify again instead of the marker suppressing it.
            with_remembered(id, |r| r.notified_fingerprint = None);
            crate::rlog!(
                ERROR,
                "ralphus [guardian] review {id} could not enqueue base-update inconclusive notice: {e}"
            );
            crate::cartographer::Note::new("guardian")
                .level(crate::logging::LogLevel::ERROR)
                .guardian(id)
                .emit(
                    &store.lock(),
                    "could not enqueue base-update inconclusive notice",
                    serde_json::json!({"error": e.to_string()}),
                );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{ForgeKind, classify_github_mergeability, classify_gitlab_mergeability};
    use crate::mock_forge::MockForge;
    use serde_json::json;

    fn need(v: PrRebaseNeed) -> &'static str {
        match v {
            PrRebaseNeed::Clean => "clean",
            PrRebaseNeed::Required(_) => "required",
            PrRebaseNeed::Computing => "computing",
            PrRebaseNeed::Unknown(_) => "unknown",
        }
    }

    #[test]
    fn github_classifier_separates_conflict_from_ci_and_policy_states() {
        for (state, want) in [
            ("dirty", "required"),
            ("behind", "required"),
            ("blocked", "clean"),
            ("unstable", "clean"),
            ("has_hooks", "clean"),
            ("draft", "clean"),
            ("clean", "clean"),
            ("unknown", "computing"),
        ] {
            let got = classify_github_mergeability(&json!({"mergeable_state": state}));
            assert_eq!(need(got.need), want, "{state}");
        }
        let got = classify_github_mergeability(&json!({}));
        assert_eq!(need(got.need), "computing");
    }

    #[test]
    fn gitlab_classifier_separates_conflict_from_ci_and_policy_states() {
        for (status, want) in [
            ("need_rebase", "required"),
            ("conflict", "required"),
            ("ci_must_pass", "clean"),
            ("not_approved", "clean"),
            ("draft_status", "clean"),
            ("mergeable", "clean"),
            ("checking", "computing"),
            ("unchecked", "computing"),
        ] {
            let got = classify_gitlab_mergeability(&json!({"detailed_merge_status": status}));
            assert_eq!(need(got.need), want, "{status}");
        }
        let got = classify_gitlab_mergeability(&json!({"has_conflicts": true}));
        assert_eq!(need(got.need), "required");
    }

    fn assessment(verdict: Verdict, sha: &str) -> Assessment {
        let mut shas = BTreeMap::new();
        shas.insert("p:upstream".to_string(), sha.to_string());
        Assessment {
            verdict,
            reasons: vec!["why".into()],
            shas,
            provisional: false,
            discarded: false,
        }
    }

    #[test]
    fn record_notifies_a_persistent_inconclusive_once() {
        let id = "test-record-notify";
        forget(id);
        let a = assessment(Verdict::Inconclusive, "aaa");
        assert_eq!(record(id, &a), Gate::Skip);
        assert_eq!(record(id, &a), Gate::Skip);
        assert_eq!(record(id, &a), Gate::SkipAndNotify);
        assert_eq!(record(id, &a), Gate::Skip);
        forget(id);
    }

    #[test]
    fn record_restarts_the_count_when_the_evidence_changes() {
        let id = "test-record-restart";
        forget(id);
        let a = assessment(Verdict::Inconclusive, "aaa");
        let b = assessment(Verdict::Inconclusive, "bbb");
        record(id, &a);
        record(id, &a);
        assert_eq!(record(id, &b), Gate::Skip);
        assert_eq!(record(id, &b), Gate::Skip);
        assert_eq!(record(id, &b), Gate::SkipAndNotify);
        forget(id);
    }

    #[test]
    fn record_does_not_count_a_discarded_result() {
        let id = "test-record-discarded";
        forget(id);
        let mut a = assessment(Verdict::Inconclusive, "aaa");
        a.discarded = true;
        for _ in 0..5 {
            assert_eq!(record(id, &a), Gate::Skip);
        }
        forget(id);
    }

    #[test]
    fn record_rebuilds_on_required_and_resets_memory() {
        let id = "test-record-required";
        forget(id);
        let a = assessment(Verdict::Inconclusive, "aaa");
        record(id, &a);
        record(id, &a);
        assert_eq!(
            record(id, &assessment(Verdict::RebaseRequired, "aaa")),
            Gate::Rebuild
        );
        assert_eq!(record(id, &a), Gate::Skip);
        forget(id);
    }

    #[test]
    fn provisional_clean_is_not_remembered_but_forge_backed_clean_is() {
        let id = "test-record-provisional";
        forget(id);
        let mut a = assessment(Verdict::Clean, "aaa");
        a.provisional = true;
        assert_eq!(record(id, &a), Gate::Skip);
        with_remembered(id, |r| assert!(r.clean_fingerprint.is_none()));
        a.provisional = false;
        assert_eq!(record(id, &a), Gate::Skip);
        with_remembered(id, |r| {
            assert_eq!(r.clean_fingerprint, Some(a.fingerprint()));
        });
        forget(id);
        with_remembered(id, |r| assert!(r.clean_fingerprint.is_none()));
        forget(id);
    }

    fn serve(
        kind: ForgeKind,
        repo: &str,
        bodies: Vec<serde_json::Value>,
    ) -> (MockForge, ForgeClient) {
        let mock = MockForge::start(move |server| {
            for body in bodies {
                let req = server.recv();
                req.respond(tiny_http::Response::from_string(body.to_string()))
                    .unwrap();
            }
        });
        let client = mock.client(kind, repo);
        (mock, client)
    }

    const TICK: Duration = Duration::from_millis(1);

    #[test]
    fn check_pr_github_conflict_requires_and_blocked_is_clean() {
        let (mock, client) = serve(
            ForgeKind::GitHub,
            "acme/widget",
            vec![
                json!({"mergeable_state":"dirty","head":{"sha":"h1"},"base":{"ref":"main"}}),
                json!({"mergeable_state":"blocked","head":{"sha":"h1"},"base":{"ref":"main"}}),
            ],
        );
        assert!(matches!(
            check_pr(&client, 1, Some("h1"), "main", 2, TICK),
            PrCheck::Required(_)
        ));
        assert_eq!(
            check_pr(&client, 1, Some("h1"), "main", 2, TICK),
            PrCheck::Clean
        );
        mock.finish();
    }

    #[test]
    fn check_pr_gitlab_retries_checking_then_reads_need_rebase() {
        let (mock, client) = serve(
            ForgeKind::GitLab,
            "acme%2Fwidget",
            vec![
                json!({"detailed_merge_status":"checking","sha":"h1","target_branch":"main"}),
                json!({"detailed_merge_status":"need_rebase","sha":"h1","target_branch":"main"}),
            ],
        );
        assert!(matches!(
            check_pr(&client, 7, Some("h1"), "main", 3, TICK),
            PrCheck::Required(_)
        ));
        mock.finish();
    }

    #[test]
    fn check_pr_rejects_a_moved_head_or_retargeted_base() {
        let (mock, client) = serve(
            ForgeKind::GitHub,
            "acme/widget",
            vec![
                json!({"mergeable_state":"dirty","head":{"sha":"other"},"base":{"ref":"main"}}),
                json!({"mergeable_state":"dirty","head":{"sha":"h1"},"base":{"ref":"feature/x"}}),
            ],
        );
        assert!(matches!(
            check_pr(&client, 1, Some("h1"), "main", 1, TICK),
            PrCheck::Inconclusive(_)
        ));
        assert!(matches!(
            check_pr(&client, 1, Some("h1"), "main", 1, TICK),
            PrCheck::Inconclusive(_)
        ));
        mock.finish();
    }

    #[test]
    fn check_pr_gives_up_when_the_forge_never_finishes_computing() {
        let (mock, client) = serve(
            ForgeKind::GitHub,
            "acme/widget",
            vec![json!({"mergeable_state":"unknown","head":{"sha":"h1"},"base":{"ref":"main"}}); 3],
        );
        assert!(matches!(
            check_pr(&client, 1, Some("h1"), "main", 3, TICK),
            PrCheck::Inconclusive(_)
        ));
        mock.finish();
    }
}
