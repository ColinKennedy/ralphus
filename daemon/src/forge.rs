//! Forge integration (RAL-117): create and query pull/merge requests against
//! GitHub or GitLab over their REST APIs directly — no `gh` CLI or other
//! external tool dependency, matching this daemon's existing habit of calling
//! provider HTTP APIs straight with `ureq` (see `chat_client.rs`).
//!
//! Routing is config-driven: [`crate::config::ForgeConfig`] (the `[forge]`
//! TOML table, global or per-project) can pin the forge kind, API base URL,
//! and token env var explicitly. Any field left unset falls back to
//! autodetection from the repository's `git remote get-url` (see
//! [`resolve_remote`]).
//!
//! `[forge].remote` is the one field that is a *fallback*, not an override:
//! which remote a review resolves against is decided by the review's own
//! `base_branch` first — see [`resolve_remote_name`] for the full four-step
//! precedence — so a repo with a fork plus an upstream opens each review's PR
//! against whichever host that review is actually based on (RAL-282).
//!
//! ## Auth (RAL-117 Q8)
//!
//! A forge API token is read from an environment variable — never from a
//! config file or the database — so credentials never touch disk under
//! ralphus's control. The variable name is `[forge].token_env` if set,
//! otherwise `RALPHUS_GITHUB_TOKEN` / `RALPHUS_GITLAB_TOKEN` depending on the
//! resolved forge kind. Missing tokens are only an error at the point a call
//! actually needs one (public-repo reads may work unauthenticated); this keeps
//! `resolve_remote` usable for kind/repo detection even when no token is
//! configured yet. Future work reusing forge auth should follow this same
//! env-var convention rather than inventing a new mechanism.
//!
//! If the env var isn't set, [`resolve_cli_token`] falls back to asking the
//! forge's own CLI (`gh auth token` / `glab auth status --show-token`) for a
//! token already cached from a prior interactive login.
//!
//! TODO: Replace with real user-service authentication once RAL-245 is complete.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::ForgeConfig;

/// Which forge a repository is hosted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForgeKind {
    GitHub,
    GitLab,
}

impl ForgeKind {
    /// The stored/config lowercase string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
        }
    }

    /// Parse from a config/stored string, case-insensitively.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "github" => Some(Self::GitHub),
            "gitlab" => Some(Self::GitLab),
            _ => None,
        }
    }

    /// Guess from a remote host, e.g. `github.com`, `gitlab.example.com`.
    ///
    /// `pub(crate)` (RAL-338) so fork registration can decide whether a
    /// fork URL's `fork_owner` should be auto-derived (GitHub) or left
    /// empty (GitLab addresses cross-project MRs by numeric id instead).
    pub(crate) fn from_host(host: &str) -> Option<Self> {
        let host = host.to_lowercase();
        if host.contains("github") {
            Some(Self::GitHub)
        } else if host.contains("gitlab") {
            Some(Self::GitLab)
        } else {
            None
        }
    }

    fn default_api_base(self, host: &str) -> String {
        match self {
            Self::GitHub if host.eq_ignore_ascii_case("github.com") => {
                "https://api.github.com".to_string()
            }
            // GitHub Enterprise Server's API lives under `/api/v3` on the same host.
            Self::GitHub => format!("https://{host}/api/v3"),
            Self::GitLab if host.eq_ignore_ascii_case("gitlab.com") => {
                "https://gitlab.com/api/v4".to_string()
            }
            Self::GitLab => format!("https://{host}/api/v4"),
        }
    }

    fn default_token_env(self) -> &'static str {
        match self {
            Self::GitHub => "RALPHUS_GITHUB_TOKEN",
            Self::GitLab => "RALPHUS_GITLAB_TOKEN",
        }
    }
}

/// One PR/MR comment ("note" in GitLab terminology), normalised across forges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrComment {
    /// Forge-native comment id (as a string; GitHub/GitLab both use integers,
    /// but the mapping is kept opaque so we never need to reparse it).
    pub external_id: String,
    /// Comment author's display/login name.
    pub author: String,
    /// Comment body (markdown).
    pub body: String,
    /// ISO-8601 creation timestamp as returned by the forge.
    pub created_at: String,
}

/// Page size requested from every paginated forge listing.
///
/// Both GitHub and GitLab cap `per_page` at 100 and default to far less (30
/// and 20). Requesting the maximum is what keeps a single page enough for all
/// but the busiest PRs -- without it, a PR's 31st comment onward was invisible
/// to `pull-feedback`, so that feedback was never actioned and the un-actioned
/// count silently under-reported.
const PER_PAGE: u32 = 100;

/// Which comment endpoint [`ForgeClient::list_pr_comments_conditional`]
/// polls (RAL-366). GitHub splits PR feedback across two REST resources --
/// general conversation (`/issues/{n}/comments`) and inline review comments
/// (`/pulls/{n}/comments`) -- so the RAL-366 cache poller fetches both.
/// GitLab's single `/notes` endpoint already returns both kinds, so `source`
/// is ignored there; the poller still only needs to call it once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrCommentEndpoint {
    Conversation,
    Review,
}

/// Result of [`ForgeClient::get_conditional`] (RAL-366).
enum ConditionalGet {
    /// The forge returned `304`: `etag` still matches, nothing to re-parse.
    NotModified,
    /// A fresh body, plus this response's own `ETag` (`None` if the forge
    /// didn't send one) to store for the next poll's `If-None-Match`.
    Modified {
        value: serde_json::Value,
        etag: Option<String>,
    },
}

/// Result of [`ForgeClient::list_pr_comments_conditional`] (RAL-366).
#[derive(Debug)]
pub enum CommentsPoll {
    /// The stored ETag still matched -- no forge quota spent, caller should
    /// keep whatever comment set it already has cached for this endpoint.
    NotModified,
    /// A fresh comment set, plus the new ETag to store for next time (`None`
    /// if this forge/response didn't send one, in which case the next poll
    /// falls back to an unconditional fetch).
    Modified {
        comments: Vec<PrComment>,
        etag: Option<String>,
    },
}

/// Live base-ref metadata used to reconcile forge-authored base edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestBaseState {
    pub base: String,
    pub updated_at_ms: i64,
}

/// One individually failing check-run (GitHub) or job (GitLab) within a PR's
/// CI (RAL-<new>) -- `PrFailure::checks` carries one of these per failing
/// check/job so a caller like `ci_watch::dispatch_pr_auto_fix` can hand an
/// agent every failing job's URL up front instead of just the first one this
/// module happens to notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    /// The check-run's `name` (GitHub) or job's `name` (GitLab).
    pub name: String,
    /// A human-clickable URL for this specific job/check, when the forge gave
    /// one.
    pub job_url: Option<String>,
    /// Best-effort raw failure text pulled from the forge for this specific
    /// job/check (a check-run's `output.text`/`output.summary` on GitHub, a
    /// job's trace on GitLab) -- `None` when the forge has none to offer.
    /// Full, untrimmed text; trimming to a mailbox-safe excerpt is
    /// `crate::ci_watch::trim_log_excerpt`'s job, not this layer's.
    pub log_text: Option<String>,
    /// The specific step *within* `name`'s job/check that actually failed,
    /// when the forge can distinguish one (RAL-<new>) -- see
    /// `.agent/forge-design-principles.md`'s "GitHub job-level ambiguity"
    /// section for the incident this exists to prevent: a GitHub Actions job
    /// can bundle several independently-tracked, differently-purposed steps
    /// under one job name (e.g. this repo's "Docs (screenshot coverage
    /// lint)" job also runs an unrelated cli-reference.md freshness check as
    /// a separate step), and `name` alone doesn't tell a caller which one
    /// actually broke. Always `None` from [`ForgeClient::check_pr_ci_status`]
    /// itself (both forges) -- populated afterward, on demand, only by a
    /// caller that's about to hand this to an agent (see
    /// `ci_watch::run_pr_fix`), since resolving it costs an extra forge call
    /// per failing check and the routine 2-minute standing poll has no need
    /// to pay that on every tick. Left `None` for GitLab today: a GitLab CI
    /// job has no equivalent "steps with their own tracked conclusion"
    /// concept, and the full job trace this module already fetches for
    /// GitLab (`log_text`) already surfaces the actual failing command in
    /// most cases, unlike GitHub's `output.text`/`output.summary`, which is
    /// usually empty.
    pub failing_step: Option<String>,
}

/// A blocker a forge reports against a PR/MR ever landing on its base branch
/// (RAL-375): a failed required check, or the forge's own merge-conflict
/// verdict. Deliberately not narrower ("did CI pass") -- `crate::ci_watch`
/// polls for anything that would stop this PR from merging or rebasing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFailure {
    /// Human-readable description of what's blocking, e.g. "check 'build'
    /// failed", "3 checks failed: 'build', 'lint', 'test'", or "merge
    /// conflicts with the base branch".
    pub reason: String,
    /// The first failing job/check's URL, when the forge gave one -- `None`
    /// for a conflict verdict, which has no job to point at. Kept alongside
    /// `checks` (rather than requiring every caller to index into it) since
    /// most existing callers (the persisted `guardian_pull_requests.ci_failure_job_url`
    /// column, the mailbox notice's summary line) only ever wanted a single
    /// representative URL.
    pub job_url: Option<String>,
    /// The first failing job/check's raw failure text, mirroring `job_url`'s
    /// "first, for single-URL callers" role. See `checks` for every failing
    /// job/check's own text.
    pub log_text: Option<String>,
    /// Every individually failing check-run/job this poll found (RAL-<new>),
    /// each with its own name/URL/log text -- empty for a failure with no
    /// per-job breakdown (a merge-conflict verdict, or a pipeline that failed
    /// before any job could be attributed). `crate::ci_watch::dispatch_pr_auto_fix`
    /// uses this to list every failing job's URL for the auto-fix agent
    /// up front, rather than making it re-derive that from a single summary.
    pub checks: Vec<FailedCheck>,
}

/// Build a [`PrFailure`] from every individually failing check/job found in
/// one poll (RAL-<new>) -- `noun` is `"check"` (GitHub) or `"job"` (GitLab)
/// so `reason` reads naturally either way. `checks` must be non-empty; the
/// conflict-verdict and no-per-job-breakdown paths construct `PrFailure`
/// directly instead of going through this helper.
fn build_pr_failure(checks: Vec<FailedCheck>, noun: &str) -> PrFailure {
    let reason = match checks.as_slice() {
        [one] => format!("{noun} '{}' failed", one.name),
        many => format!(
            "{} {noun}s failed: {}",
            many.len(),
            many.iter()
                .map(|c| format!("'{}'", c.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let job_url = checks.first().and_then(|c| c.job_url.clone());
    let log_text = checks.first().and_then(|c| c.log_text.clone());
    PrFailure {
        reason,
        job_url,
        log_text,
        checks,
    }
}

/// The live outcome of polling a PR/MR's CI + mergeability (RAL-375, see
/// [`ForgeClient::check_pr_ci_status`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrCiState {
    /// No terminal signal yet -- checks still running, or the forge hasn't
    /// reported a mergeability verdict yet. Keep polling.
    Pending,
    /// Every required check passed and the forge reports no merge blocker.
    Passing,
    /// Something would block this PR from merging or rebasing onto its base.
    Failing(PrFailure),
}

impl PrCiState {
    /// The stored/API string -- `"pending"`/`"passing"`/`"failing"` (RAL-395:
    /// persisted on `guardian_pull_requests.ci_status` so the board can read
    /// a PR's last-polled CI state without a live forge call).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passing => "passing",
            Self::Failing(_) => "failing",
        }
    }
}

/// A CI result paired with the forge's current draft/WIP flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCiProbe {
    pub ci: PrCiState,
    pub draft: bool,
}

fn pr_object_draft(obj: &serde_json::Value) -> bool {
    obj["draft"]
        .as_bool()
        .or_else(|| obj["work_in_progress"].as_bool())
        .unwrap_or(false)
}

/// A created pull/merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPr {
    /// PR number (GitHub) / MR `iid` (GitLab) — repo-scoped, not a global id.
    pub number: i64,
    /// Web URL a human can open.
    pub url: String,
    /// Whether the forge created the PR/MR as a draft.
    pub draft: bool,
}

/// An already-open PR/MR discovered via [`ForgeClient::find_open_pull_request`]
/// (RAL-<new>) — distinct from [`CreatedPr`] because it also carries the
/// PR/MR's current base/title/body, so a caller adopting it can record what
/// the forge actually has rather than what ralphus intended to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingPr {
    pub number: i64,
    pub url: String,
    /// Whether the already-open PR/MR is a draft.
    pub draft: bool,
    /// The base ref (GitHub) / target branch (GitLab) currently recorded on
    /// the forge — may differ from what a caller was about to request; the
    /// normal base-resync path reconciles that afterward.
    pub base: String,
    pub title: String,
    /// Empty when the PR/MR has no description, not absent.
    pub description: String,
}

/// A registered GitHub-native PR stack (`GET/POST .../stacks`) — GitHub only,
/// see [`ForgeClient::create_stack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatedStack {
    /// Repo-scoped stack number, not a global id.
    pub number: i64,
}

/// A resolved connection to one forge repository: enough to create PRs, list
/// comments, and fetch a PR template. Built by [`resolve_remote`].
#[derive(Clone)]
pub struct ForgeClient {
    kind: ForgeKind,
    api_base: String,
    /// `owner/repo` for GitHub; URL-percent-encoded `namespace%2Fproject` path
    /// for GitLab (its REST API addresses projects by encoded path or numeric id).
    repo_path: String,
    token: Option<String>,
    /// The host [`resolve_cli_token_cached`] would have used to cache
    /// `token`, if it came from that path -- set by
    /// [`Self::with_cli_token_host`], never by [`Self::new`] directly (test
    /// callers construct a client with no cache to evict).
    ///
    /// A 401/403 response on this client evicts `CLI_TOKEN_CACHE`'s entry for
    /// `(kind, host)` (see [`Self::describe_evicting`]) so the next resolve
    /// re-checks the CLI immediately instead of serving the same bad token
    /// for up to [`CLI_TOKEN_TTL`]. Harmless to set even when `token` came
    /// from an env var instead: evicting an absent/irrelevant cache entry is
    /// a no-op.
    cli_token_host: Option<String>,
}

impl ForgeClient {
    /// Build a client directly (mainly for tests); prefer [`resolve_remote`]
    /// in real code so kind/repo/token are derived consistently.
    #[must_use]
    pub fn new(
        kind: ForgeKind,
        api_base: String,
        repo_path: String,
        token: Option<String>,
    ) -> Self {
        Self {
            kind,
            api_base,
            repo_path,
            token,
            cli_token_host: None,
        }
    }

    /// Tags this client with the host its `token` was resolved against, so a
    /// 401/403 it receives can evict the matching [`CLI_TOKEN_CACHE`] entry.
    /// See the field doc on [`Self::cli_token_host`].
    #[must_use]
    fn with_cli_token_host(mut self, host: impl Into<String>) -> Self {
        self.cli_token_host = Some(host.into());
        self
    }

    /// Which forge this client talks to.
    #[must_use]
    pub fn kind(&self) -> ForgeKind {
        self.kind
    }

    /// `owner/repo` (GitHub) or encoded namespace path (GitLab) this client
    /// operates on — stored verbatim on `guardian_pull_requests.repo`.
    #[must_use]
    pub fn repo_label(&self) -> &str {
        &self.repo_path
    }

    /// The `head` value to use when this client's own repo owns both the
    /// branch and the PR/MR -- i.e. everywhere except the GitHub cross-repo
    /// fork root, which already builds its own `owner:branch` head from the
    /// fork's registered owner. GitHub's `pulls` list endpoint silently
    /// ignores a bare branch name in its `head` filter (it returns every
    /// open PR unfiltered instead of erroring or matching nothing), so a
    /// same-repo GitHub head must still carry the `owner:` prefix or
    /// [`Self::find_open_pull_request`] adopts an unrelated open PR;
    /// GitLab's `source_branch` filter has no such requirement.
    #[must_use]
    pub fn same_repo_head(&self, alias: &str) -> String {
        match self.kind {
            ForgeKind::GitHub => {
                let owner = self.repo_path.split('/').next().unwrap_or(&self.repo_path);
                format!("{owner}:{alias}")
            }
            ForgeKind::GitLab => alias.to_string(),
        }
    }

    fn require_token(&self) -> Result<&str, String> {
        #[cfg(test)]
        if self.token.is_none() && self.api_base.starts_with("http://127.0.0.1:") {
            return Ok("test-token");
        }
        self.token.as_deref().ok_or_else(|| {
            format!(
                "no {} token configured (set ${})",
                self.kind.as_str(),
                self.kind.default_token_env()
            )
        })
    }

    /// Create a pull/merge request. `head` is the source branch (already
    /// pushed to the remote); `base` is the target branch/ref it merges into.
    /// Logs the outbound call (start/done/error) via `rlog!`, matching
    /// `chat_client::call_direct`'s boundary-logging convention for direct
    /// provider HTTP calls.
    pub fn create_pull_request(
        &self,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Result<CreatedPr, String> {
        self.create_pull_request_routed(title, body, head, base, None)
    }

    /// Like [`Self::create_pull_request`], but additionally accepts a GitLab
    /// `target_project_id` (RAL-338): the numeric id of the *parent* project
    /// a cross-project MR filed on this (fork) client should target. `None`
    /// reproduces [`Self::create_pull_request`]'s exact payload -- a
    /// fork-mode root PR/MR is the only caller that ever passes `Some`.
    /// GitHub ignores this parameter: a cross-repo PR there is expressed
    /// entirely through an `owner:branch`-prefixed `head` called against the
    /// *parent's* own client instead (see `PrRoute`), never through a field
    /// on the request body.
    pub fn create_pull_request_routed(
        &self,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
        target_project_id: Option<i64>,
    ) -> Result<CreatedPr, String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [forge] create pr start kind={} repo={} head={head} base={base}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.create_pull_request_inner(title, body, head, base, target_project_id);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(pr) => crate::rlog!(
                INFO,
                "ralphus [forge] create pr done kind={} repo={} number={} url={}",
                self.kind.as_str(),
                self.repo_path,
                pr.number,
                pr.url
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] create pr failed kind={} repo={}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn create_pull_request_inner(
        &self,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
        target_project_id: Option<i64>,
    ) -> Result<CreatedPr, String> {
        let token = self.require_token()?;
        match self.kind {
            ForgeKind::GitHub => {
                let url = format!("{}/repos/{}/pulls", self.api_base, self.repo_path);
                let payload = serde_json::json!({
                    "title": title,
                    "body": body,
                    "head": head,
                    "base": base,
                });
                let resp = self.send(
                    ureq::post(&url)
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Accept", "application/vnd.github+json"),
                    &payload,
                )?;
                let number = resp["number"]
                    .as_i64()
                    .ok_or_else(|| format!("unexpected GitHub PR response shape: {resp}"))?;
                let url = resp["html_url"].as_str().unwrap_or_default().to_string();
                Ok(CreatedPr {
                    number,
                    url,
                    draft: pr_object_draft(&resp),
                })
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests",
                    self.api_base, self.repo_path
                );
                let mut payload = serde_json::json!({
                    "source_branch": head,
                    "target_branch": base,
                    "title": title,
                    "description": body,
                });
                if let Some(id) = target_project_id {
                    payload["target_project_id"] = serde_json::json!(id);
                }
                let resp = self.send(ureq::post(&url).set("PRIVATE-TOKEN", token), &payload)?;
                let number = resp["iid"]
                    .as_i64()
                    .ok_or_else(|| format!("unexpected GitLab MR response shape: {resp}"))?;
                let url = resp["web_url"].as_str().unwrap_or_default().to_string();
                Ok(CreatedPr {
                    number,
                    url,
                    draft: pr_object_draft(&resp),
                })
            }
        }
    }

    /// Look up whether an open PR/MR already exists for `head` against this
    /// repo, via the forge's own documented head/source-branch filter query
    /// (RAL-<new>) -- GitHub's `GET .../pulls?head=&state=open`, GitLab's
    /// `GET .../merge_requests?source_branch=&state=opened`. Deliberately
    /// never inspects a creation attempt's error text: GitHub's "a pull
    /// request already exists" validation failure carries no stable
    /// machine-readable code of its own (`code: "custom"`, like every other
    /// free-text validation message), so a caller wanting to detect it durably
    /// must ask the API directly instead of pattern-matching that wording.
    /// `Ok(None)` when no open PR/MR has this head.
    ///
    /// # Errors
    /// Missing token or any forge API/parse failure.
    pub fn find_open_pull_request(&self, head: &str) -> Result<Option<ExistingPr>, String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] find open pr start kind={} repo={} head={head}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.find_open_pull_request_inner(head);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(Some(found)) => crate::rlog!(
                INFO,
                "ralphus [forge] find open pr found kind={} repo={} head={head} number={} \
                 base={}",
                self.kind.as_str(),
                self.repo_path,
                found.number,
                found.base
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(None) => crate::rlog!(
                DEBUG,
                "ralphus [forge] find open pr none kind={} repo={} head={head}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] find open pr failed kind={} repo={} head={head}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn find_open_pull_request_inner(&self, head: &str) -> Result<Option<ExistingPr>, String> {
        let token = self.require_token()?;
        let found = match self.kind {
            ForgeKind::GitHub => {
                let url = format!("{}/repos/{}/pulls", self.api_base, self.repo_path);
                let resp = self.get(
                    ureq::get(&url)
                        .query("head", head)
                        .query("state", "open")
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Accept", "application/vnd.github+json"),
                )?;
                let arr = resp
                    .as_array()
                    .ok_or_else(|| format!("unexpected GitHub PR list response shape: {resp}"))?;
                let Some(found) = arr.first() else {
                    return Ok(None);
                };
                let number = found["number"]
                    .as_i64()
                    .ok_or_else(|| format!("unexpected GitHub PR response shape: {found}"))?;
                let url = found["html_url"].as_str().unwrap_or_default().to_string();
                let base = found["base"]["ref"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "forge response missing base.ref".to_string())?;
                let title = found["title"].as_str().unwrap_or_default().to_string();
                let description = found["body"].as_str().unwrap_or_default().to_string();
                ExistingPr {
                    number,
                    url,
                    draft: pr_object_draft(found),
                    base,
                    title,
                    description,
                }
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests",
                    self.api_base, self.repo_path
                );
                let resp = self.get(
                    ureq::get(&url)
                        .query("source_branch", head)
                        .query("state", "opened")
                        .set("PRIVATE-TOKEN", token),
                )?;
                let arr = resp
                    .as_array()
                    .ok_or_else(|| format!("unexpected GitLab MR list response shape: {resp}"))?;
                let Some(found) = arr.first() else {
                    return Ok(None);
                };
                let number = found["iid"]
                    .as_i64()
                    .ok_or_else(|| format!("unexpected GitLab MR response shape: {found}"))?;
                let url = found["web_url"].as_str().unwrap_or_default().to_string();
                let base = found["target_branch"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "forge response missing target_branch".to_string())?;
                let title = found["title"].as_str().unwrap_or_default().to_string();
                let description = found["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                ExistingPr {
                    number,
                    url,
                    draft: pr_object_draft(found),
                    base,
                    title,
                    description,
                }
            }
        };
        Ok(Some(found))
    }

    /// Resolve this GitLab project's numeric id via `GET /projects/{path}`
    /// (RAL-338) -- needed as the `target_project_id` of a cross-project MR
    /// filed from a fork against its parent, since GitLab's merge request
    /// API addresses the target project numerically, not by path. Callers
    /// should resolve this once per submit and reuse it across every branch
    /// in the stack rather than re-resolving per PR.
    ///
    /// # Errors
    /// Returns `Err` on a GitHub client (this concept doesn't apply there),
    /// a missing token, or any forge API/parse failure.
    pub fn resolve_gitlab_project_id(&self) -> Result<i64, String> {
        if self.kind != ForgeKind::GitLab {
            return Err("resolve_gitlab_project_id is only meaningful for GitLab".to_string());
        }
        let token = self.require_token()?;
        let url = format!("{}/projects/{}", self.api_base, self.repo_path);
        let resp = self.get(ureq::get(&url).set("PRIVATE-TOKEN", token))?;
        resp["id"]
            .as_i64()
            .ok_or_else(|| format!("unexpected GitLab project response shape: {resp}"))
    }

    /// Fetch a PR/MR's *live* state from the forge, normalized to ralphus's
    /// own `"open"`/`"merged"`/`"closed"` convention
    /// ([`PullRequestView::state`]) -- so a PR closed or merged outside
    /// ralphus (the GitHub/GitLab UI, `gh pr close`, ...) can be detected
    /// instead of trusting a locally-recorded `state` that may be stale
    /// forever (RAL-190+: without this, a review whose PR was closed
    /// externally looks "already submitted" and a fresh "submit stack" call
    /// silently does nothing). Logs the outbound call (start/done/error) via
    /// `rlog!`.
    pub fn get_pull_request_state(&self, number: i64) -> Result<String, String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] get pr state start kind={} repo={} number={number}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.get_pull_request_state_inner(number);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(state) => crate::rlog!(
                DEBUG,
                "ralphus [forge] get pr state done kind={} repo={} number={number} state={state}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] get pr state failed kind={} repo={} number={number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn get_pull_request_state_inner(&self, number: i64) -> Result<String, String> {
        let token = self.require_token()?;
        match self.kind {
            ForgeKind::GitHub => {
                let url = format!("{}/repos/{}/pulls/{number}", self.api_base, self.repo_path);
                let resp = self.get(
                    ureq::get(&url)
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Accept", "application/vnd.github+json"),
                )?;
                if resp["merged"].as_bool().unwrap_or(false) {
                    return Ok("merged".to_string());
                }
                Ok(resp["state"].as_str().unwrap_or("open").to_string())
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{number}",
                    self.api_base, self.repo_path
                );
                let resp = self.get(ureq::get(&url).set("PRIVATE-TOKEN", token))?;
                Ok(match resp["state"].as_str().unwrap_or("opened") {
                    "opened" => "open".to_string(),
                    other => other.to_string(),
                })
            }
        }
    }

    /// Fetch a PR/MR's *live* base/target branch from the forge (RAL-273,
    /// RAL-279), unprefixed (GitHub's `base.ref` / GitLab's `target_branch`
    /// are both already bare branch names, matching the convention
    /// [`Self::update_pull_request_base`] writes and `base_ref` is stored
    /// under). Used both to detect a stack reordered on the forge's own UI
    /// (dragging PRs into a new order, which retargets each PR's base)
    /// instead of only ever pushing ralphus's local order out via
    /// [`Self::update_pull_request_base`], and by the forge-to-ralphus drift
    /// poll to detect a base retargeted directly on the forge (a reviewer
    /// changed it, or an intervening branch's PR was closed/merged there).
    /// Logs the outbound call (start/done/error) via `rlog!`.
    pub fn get_pull_request_base(&self, number: i64) -> Result<String, String> {
        self.get_pull_request_base_state(number).map(|s| s.base)
    }

    /// Fetch the live base and the forge timestamp of the edit. The timestamp
    /// is required for RAL-277's cross-system last-write-wins rule.
    pub fn get_pull_request_base_state(&self, number: i64) -> Result<PullRequestBaseState, String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] get pr base start kind={} repo={} number={number}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.get_pull_request_base_state_inner(number);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(state) => crate::rlog!(
                DEBUG,
                "ralphus [forge] get pr base done kind={} repo={} number={number} base={} updated_at_ms={}",
                self.kind.as_str(),
                self.repo_path,
                state.base,
                state.updated_at_ms
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] get pr base failed kind={} repo={} number={number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn get_pull_request_base_state_inner(
        &self,
        number: i64,
    ) -> Result<PullRequestBaseState, String> {
        let token = self.require_token()?;
        let (base, updated_at) = match self.kind {
            ForgeKind::GitHub => {
                let url = format!("{}/repos/{}/pulls/{number}", self.api_base, self.repo_path);
                let resp = self.get(
                    ureq::get(&url)
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Accept", "application/vnd.github+json"),
                )?;
                let base = resp["base"]["ref"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "forge response missing base.ref".to_string())?;
                let updated_at = resp["updated_at"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "forge response missing updated_at".to_string())?;
                (base, updated_at)
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{number}",
                    self.api_base, self.repo_path
                );
                let resp = self.get(ureq::get(&url).set("PRIVATE-TOKEN", token))?;
                let base = resp["target_branch"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "forge response missing target_branch".to_string())?;
                let updated_at = resp["updated_at"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "forge response missing updated_at".to_string())?;
                (base, updated_at)
            }
        };
        let updated_at_ms = chrono::DateTime::parse_from_rfc3339(&updated_at)
            .map_err(|e| format!("forge response has invalid updated_at {updated_at:?}: {e}"))?
            .timestamp_millis();
        Ok(PullRequestBaseState {
            base,
            updated_at_ms,
        })
    }

    /// Poll a PR/MR's live CI + mergeability status (RAL-375): the full set
    /// of merge/rebase blockers the forge surfaces, not narrowly "did CI
    /// pass" -- a failed required check-run/pipeline job, or a forge-verdict
    /// merge conflict, both come back as [`PrCiState::Failing`]. Logs the
    /// outbound call (start/done/error) via `rlog!`.
    pub fn check_pr_ci_status(&self, number: i64) -> Result<PrCiState, String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] check pr ci status start kind={} repo={} number={number}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.check_pr_ci_status_inner(number);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(state) => crate::rlog!(
                DEBUG,
                "ralphus [forge] check pr ci status done kind={} repo={} number={number} state={state:?}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] check pr ci status failed kind={} repo={} number={number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    /// Poll CI and the forge's draft/WIP flag.
    pub fn check_pr_ci_status_probe(&self, number: i64) -> Result<PrCiProbe, String> {
        let ci = self.check_pr_ci_status(number)?;
        let token = self.require_token()?;
        let object = match self.kind {
            ForgeKind::GitHub => {
                let url = format!("{}/repos/{}/pulls/{number}", self.api_base, self.repo_path);
                self.get(
                    ureq::get(&url)
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Accept", "application/vnd.github+json"),
                )?
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{number}",
                    self.api_base, self.repo_path
                );
                self.get(ureq::get(&url).set("PRIVATE-TOKEN", token))?
            }
        };
        Ok(PrCiProbe {
            ci,
            draft: pr_object_draft(&object),
        })
    }

    fn check_pr_ci_status_inner(&self, number: i64) -> Result<PrCiState, String> {
        match self.kind {
            ForgeKind::GitHub => self.check_github_pr_ci_status(number),
            ForgeKind::GitLab => self.check_gitlab_pr_ci_status(number),
        }
    }

    /// GitHub half of [`Self::check_pr_ci_status`]: `mergeable_state` for the
    /// conflict verdict (`"dirty"` -- GitHub's own term for "has conflicts"),
    /// then the head commit's check-runs for CI. `"blocked"`/`"behind"`/
    /// `"unknown"` are treated as pending rather than failing -- they mean
    /// GitHub hasn't finished computing mergeability yet (`"unknown"`) or a
    /// branch-protection rule wants something other than a check result
    /// (`"blocked"`/`"behind"`), neither of which this poll can act on
    /// itself; a genuinely failing required check still surfaces via the
    /// check-runs scan below regardless of `mergeable_state`.
    ///
    /// After check-runs all report `completed`, this also consults the
    /// legacy combined-status endpoint (`.../commits/{sha}/status`) as a
    /// belt-and-suspenders check for the older commit-status API -- but that
    /// endpoint's `state` defaults to `"pending"` whenever the commit has
    /// zero legacy statuses at all (`total_count: 0`), which is the normal
    /// case for any repo (like this one) whose CI reports exclusively
    /// through the Checks API. That default must be told apart from a real
    /// in-flight legacy status (`total_count > 0`) -- conflating them once
    /// made every such PR report `Pending` forever, no matter how green its
    /// check-runs were.
    fn check_github_pr_ci_status(&self, number: i64) -> Result<PrCiState, String> {
        let token = self.require_token()?;
        let pr_url = format!("{}/repos/{}/pulls/{number}", self.api_base, self.repo_path);
        let pr = self.get(
            ureq::get(&pr_url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Accept", "application/vnd.github+json"),
        )?;
        if pr["mergeable_state"].as_str() == Some("dirty") {
            return Ok(PrCiState::Failing(PrFailure {
                reason: "merge conflicts with the base branch".to_string(),
                job_url: None,
                log_text: None,
                checks: vec![],
            }));
        }
        let Some(sha) = pr["head"]["sha"].as_str() else {
            return Ok(PrCiState::Pending);
        };

        let checks_url = format!(
            "{}/repos/{}/commits/{sha}/check-runs",
            self.api_base, self.repo_path
        );
        let checks = self.get(
            ureq::get(&checks_url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Accept", "application/vnd.github+json"),
        )?;
        let runs = checks["check_runs"].as_array().cloned().unwrap_or_default();
        let failing: Vec<FailedCheck> = runs
            .iter()
            .filter(|run| {
                matches!(
                    run["conclusion"].as_str().unwrap_or_default(),
                    "failure" | "timed_out" | "cancelled" | "action_required"
                )
            })
            .map(|run| FailedCheck {
                name: run["name"].as_str().unwrap_or("check").to_string(),
                job_url: run["details_url"]
                    .as_str()
                    .or_else(|| run["html_url"].as_str())
                    .map(str::to_string),
                log_text: run["output"]["text"]
                    .as_str()
                    .or_else(|| run["output"]["summary"].as_str())
                    .map(str::to_string),
                failing_step: None,
            })
            .collect();
        if !failing.is_empty() {
            return Ok(PrCiState::Failing(build_pr_failure(failing, "check")));
        }
        if runs
            .iter()
            .any(|r| r["status"].as_str() != Some("completed"))
        {
            return Ok(PrCiState::Pending);
        }

        let status_url = format!(
            "{}/repos/{}/commits/{sha}/status",
            self.api_base, self.repo_path
        );
        let status = self.get(
            ureq::get(&status_url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Accept", "application/vnd.github+json"),
        )?;
        if matches!(status["state"].as_str(), Some("failure") | Some("error")) {
            let failing: Vec<FailedCheck> = status["statuses"]
                .as_array()
                .map(|s| {
                    s.iter()
                        .filter(|c| matches!(c["state"].as_str(), Some("failure") | Some("error")))
                        .map(|c| FailedCheck {
                            name: c["context"].as_str().unwrap_or("status").to_string(),
                            job_url: c["target_url"].as_str().map(str::to_string),
                            log_text: None,
                            failing_step: None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Ok(PrCiState::Failing(if failing.is_empty() {
                PrFailure {
                    reason: "a required status check failed".to_string(),
                    job_url: None,
                    log_text: None,
                    checks: vec![],
                }
            } else {
                build_pr_failure(failing, "status")
            }));
        }
        // GitHub's combined-status endpoint defaults `state` to `"pending"`
        // whenever the commit has zero legacy commit statuses at all
        // (`total_count: 0`, `statuses: []`) -- this is GitHub's documented
        // behavior for that endpoint, not a transient/in-flight signal. A repo
        // whose CI reports exclusively through the Checks API (as this one
        // does: every job above comes back as a check-run, never a legacy
        // status) will *always* get `total_count: 0` here, so treating that
        // default the same as a real pending status made every such PR stick
        // at `Pending` forever -- the check-runs gate above had already
        // confirmed everything completed, but this fallthrough overrode it on
        // every poll. Only an actual pending legacy status (`total_count > 0`)
        // should hold up the verdict; zero statuses means there is nothing
        // more to check, so fall through to `Passing`.
        if status["total_count"].as_i64().unwrap_or(0) > 0
            && status["state"].as_str() == Some("pending")
        {
            return Ok(PrCiState::Pending);
        }

        Ok(PrCiState::Passing)
    }

    /// GitLab half of [`Self::check_pr_ci_status`]: `merge_status` for the
    /// conflict verdict, then the MR's pipeline + (on failure) that
    /// pipeline's failed job trace for CI. GitLab reports `merge_status` as
    /// `"cannot_be_merged"` for a real conflict; other non-`"can_be_merged"`
    /// values (`"unchecked"`, `"checking"`) mean GitLab hasn't finished
    /// computing it yet, so they're treated as pending, not failing.
    ///
    /// RAL-462: unlike GitHub's check-runs/status calls (each scoped to a
    /// specific commit sha by its own URL), the MR endpoint's `pipeline`
    /// field is simply "the latest pipeline for this MR" -- if the MR's head
    /// just moved (rebase, force-push, a new feedback commit) and GitLab
    /// hasn't registered a pipeline for that new commit yet, this field still
    /// holds the *previous* commit's pipeline, verdict and all. Reusing that
    /// verdict verbatim would let a stale "failed" badge survive a rebase
    /// until GitLab gets around to creating the new pipeline. Comparing the
    /// pipeline's own `sha` against the MR's current head sha (both in the
    /// same response, so no extra state to track) tells the two apart: a
    /// mismatch means the pipeline belongs to a commit that's no longer the
    /// head, so there's genuinely no verdict yet for the current one.
    fn check_gitlab_pr_ci_status(&self, number: i64) -> Result<PrCiState, String> {
        let token = self.require_token()?;
        let mr_url = format!(
            "{}/projects/{}/merge_requests/{number}",
            self.api_base, self.repo_path
        );
        let mr = self.get(ureq::get(&mr_url).set("PRIVATE-TOKEN", token))?;
        if mr["merge_status"].as_str() == Some("cannot_be_merged") {
            return Ok(PrCiState::Failing(PrFailure {
                reason: "merge conflicts with the target branch".to_string(),
                job_url: None,
                log_text: None,
                checks: vec![],
            }));
        }

        let Some(pipeline_status) = mr["pipeline"]["status"].as_str() else {
            // No pipeline has run against this MR yet.
            return Ok(PrCiState::Pending);
        };
        let head_sha = mr["sha"]
            .as_str()
            .or_else(|| mr["diff_refs"]["head_sha"].as_str());
        if let (Some(head_sha), Some(pipeline_sha)) = (head_sha, mr["pipeline"]["sha"].as_str()) {
            if head_sha != pipeline_sha {
                return Ok(PrCiState::Pending);
            }
        }
        match pipeline_status {
            "success" => Ok(PrCiState::Passing),
            "failed" => {
                let Some(pipeline_id) = mr["pipeline"]["id"].as_i64() else {
                    return Ok(PrCiState::Failing(PrFailure {
                        reason: "pipeline failed".to_string(),
                        job_url: mr["pipeline"]["web_url"].as_str().map(str::to_string),
                        log_text: None,
                        checks: vec![],
                    }));
                };
                let jobs_url = format!(
                    "{}/projects/{}/pipelines/{pipeline_id}/jobs?scope[]=failed",
                    self.api_base, self.repo_path
                );
                let jobs = self
                    .get(ureq::get(&jobs_url).set("PRIVATE-TOKEN", token))
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default();
                if jobs.is_empty() {
                    return Ok(PrCiState::Failing(PrFailure {
                        reason: "pipeline failed".to_string(),
                        job_url: mr["pipeline"]["web_url"].as_str().map(str::to_string),
                        log_text: None,
                        checks: vec![],
                    }));
                }
                // One `gitlab_job_trace` call per failed job -- deliberately
                // fetched for every one of them, not just the first: this
                // poll already knows exactly which jobs failed, so paying a
                // few more sequential GitLab calls here means the auto-fix
                // agent gets every failing job's full trace up front instead
                // of having to go find it itself.
                let checks: Vec<FailedCheck> = jobs
                    .iter()
                    .map(|job| {
                        let job_id = job["id"].as_i64();
                        FailedCheck {
                            name: job["name"].as_str().unwrap_or("job").to_string(),
                            job_url: job["web_url"].as_str().map(str::to_string),
                            log_text: job_id.and_then(|id| self.gitlab_job_trace(id, token).ok()),
                            // GitLab has no step-level equivalent to resolve here --
                            // see `FailedCheck::failing_step`'s doc comment.
                            failing_step: None,
                        }
                    })
                    .collect();
                Ok(PrCiState::Failing(build_pr_failure(checks, "job")))
            }
            "canceled" | "skipped" => Ok(PrCiState::Failing(PrFailure {
                reason: format!("pipeline {pipeline_status}"),
                job_url: mr["pipeline"]["web_url"].as_str().map(str::to_string),
                log_text: None,
                checks: vec![],
            })),
            // "running" | "pending" | "created" | "waiting_for_resource" | "preparing" | ...
            _ => Ok(PrCiState::Pending),
        }
    }

    /// Best-effort lookup of which step inside a GitHub Actions job actually
    /// failed (RAL-<new>), given that job's `details_url`/`html_url` (the
    /// same value [`FailedCheck::job_url`] already carries for it). See
    /// [`FailedCheck::failing_step`]'s doc comment and
    /// `.agent/forge-design-principles.md`'s "GitHub job-level ambiguity"
    /// section for the incident this exists to prevent: GitHub's Checks API
    /// check-run (what [`Self::check_pr_ci_status`] already reads) reports
    /// only the *job's* name and usually an empty `output.text`/
    /// `output.summary` -- it says nothing about which of the job's several
    /// independently-tracked steps actually broke. This makes one extra call
    /// to the Actions API's job endpoint (`GET .../actions/jobs/{id}`, which
    /// does expose a `steps` array with per-step `conclusion`) specifically
    /// to answer that question, since paying for it on every routine
    /// CI-status poll (every 2 minutes, for every open PR) would be wasteful
    /// -- callers use this only once they're actually about to hand a
    /// failure to an agent (`ci_watch::run_pr_fix`).
    ///
    /// No-op (`None`) for a non-GitHub client, a `job_url` this can't parse a
    /// job id out of, or any forge-call/parse failure -- this is a
    /// nice-to-have enrichment of an already-known failure, never a reason to
    /// block dispatching a fix over it.
    pub fn github_failing_step(&self, job_url: &str) -> Option<String> {
        if self.kind != ForgeKind::GitHub {
            return None;
        }
        let job_id: i64 = job_url
            .split("/job/")
            .nth(1)?
            .split(['/', '?', '#'])
            .next()?
            .parse()
            .ok()?;
        let token = self.require_token().ok()?;
        let url = format!(
            "{}/repos/{}/actions/jobs/{job_id}",
            self.api_base, self.repo_path
        );
        let job = self
            .get(
                ureq::get(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Accept", "application/vnd.github+json"),
            )
            .ok()?;
        job["steps"]
            .as_array()?
            .iter()
            .find(|s| {
                matches!(
                    s["conclusion"].as_str().unwrap_or_default(),
                    "failure" | "timed_out" | "cancelled" | "action_required"
                )
            })
            .and_then(|s| s["name"].as_str())
            .map(str::to_string)
    }

    /// Raw text of a GitLab job's trace log (`GET .../jobs/{id}/trace`) --
    /// unlike every other GitLab call in this file, the response body is
    /// plain text, not JSON, so this bypasses [`get`]/[`parse_body`].
    fn gitlab_job_trace(&self, job_id: i64, token: &str) -> Result<String, String> {
        let url = format!(
            "{}/projects/{}/jobs/{job_id}/trace",
            self.api_base, self.repo_path
        );
        ureq::get(&url)
            .set("PRIVATE-TOKEN", token)
            .call()
            .map_err(|e| self.describe_evicting(e))?
            .into_string()
            .map_err(|e| format!("forge API read: {e}"))
    }

    /// Retarget an already-open PR/MR's base/target branch (RAL-190: keeps a
    /// stacked PR's base in sync after its review is reordered). Best-effort
    /// from the caller's point of view -- callers should log-and-continue on
    /// error rather than aborting a whole reorder over one forge call, since
    /// the local `base_ref` record is updated regardless. Logs the outbound
    /// call (start/done/error) via `rlog!`.
    pub fn update_pull_request_base(&self, number: i64, new_base: &str) -> Result<(), String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [forge] update pr base start kind={} repo={} number={number} new_base={new_base}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.update_pull_request_base_inner(number, new_base);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(()) => crate::rlog!(
                INFO,
                "ralphus [forge] update pr base done kind={} repo={} number={number}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] update pr base failed kind={} repo={} number={number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn update_pull_request_base_inner(&self, number: i64, new_base: &str) -> Result<(), String> {
        let token = self.require_token()?;
        match self.kind {
            ForgeKind::GitHub => {
                let url = format!("{}/repos/{}/pulls/{number}", self.api_base, self.repo_path);
                let payload = serde_json::json!({ "base": new_base });
                self.send(
                    ureq::patch(&url)
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Accept", "application/vnd.github+json"),
                    &payload,
                )?;
                Ok(())
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{number}",
                    self.api_base, self.repo_path
                );
                let payload = serde_json::json!({ "target_branch": new_base });
                self.send(ureq::put(&url).set("PRIVATE-TOKEN", token), &payload)?;
                Ok(())
            }
        }
    }

    /// Close a PR/MR without merging it (RAL-338): used by reconcile-first
    /// promotion to retire a fork-internal PR once its branch becomes the
    /// stack's new cross-repository root and a fresh PR is filed against the
    /// parent instead. Logs the outbound call (start/done/error) via
    /// `rlog!`.
    pub fn close_pull_request(&self, number: i64) -> Result<(), String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [forge] close pr start kind={} repo={} number={number}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.close_pull_request_inner(number);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(()) => crate::rlog!(
                INFO,
                "ralphus [forge] close pr done kind={} repo={} number={number}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] close pr failed kind={} repo={} number={number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn close_pull_request_inner(&self, number: i64) -> Result<(), String> {
        let token = self.require_token()?;
        match self.kind {
            ForgeKind::GitHub => {
                let url = format!("{}/repos/{}/pulls/{number}", self.api_base, self.repo_path);
                let payload = serde_json::json!({ "state": "closed" });
                self.send(
                    ureq::patch(&url)
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Accept", "application/vnd.github+json"),
                    &payload,
                )?;
                Ok(())
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{number}",
                    self.api_base, self.repo_path
                );
                let payload = serde_json::json!({ "state_event": "close" });
                self.send(ureq::put(&url).set("PRIVATE-TOKEN", token), &payload)?;
                Ok(())
            }
        }
    }

    /// Post a comment on a PR/MR (RAL-338): used by reconcile-first
    /// promotion to leave a pointer from a superseded fork-internal PR to
    /// its replacement cross-repository PR. Logs the outbound call
    /// (start/done/error) via `rlog!`.
    pub fn post_pr_comment(&self, number: i64, body: &str) -> Result<(), String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [forge] post pr comment start kind={} repo={} number={number}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.post_pr_comment_inner(number, body);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(()) => crate::rlog!(
                INFO,
                "ralphus [forge] post pr comment done kind={} repo={} number={number}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] post pr comment failed kind={} repo={} number={number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn post_pr_comment_inner(&self, number: i64, body: &str) -> Result<(), String> {
        let token = self.require_token()?;
        match self.kind {
            ForgeKind::GitHub => {
                let url = format!(
                    "{}/repos/{}/issues/{number}/comments",
                    self.api_base, self.repo_path
                );
                let payload = serde_json::json!({ "body": body });
                self.send(
                    ureq::post(&url)
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Accept", "application/vnd.github+json"),
                    &payload,
                )?;
                Ok(())
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{number}/notes",
                    self.api_base, self.repo_path
                );
                let payload = serde_json::json!({ "body": body });
                self.send(ureq::post(&url).set("PRIVATE-TOKEN", token), &payload)?;
                Ok(())
            }
        }
    }

    /// Register an ordered (bottom-to-top) list of already-created PR numbers
    /// as a GitHub-native PR stack
    /// (`POST /repos/{owner}/{repo}/stacks`, see
    /// https://docs.github.com/en/rest/pulls/stacks) so GitHub's own UI shows
    /// them as a linked stack. Each PR's base ref must already match the
    /// previous PR's head ref -- exactly the chained-base invariant
    /// [`crate::pr::submit_pull_requests`] already maintains, so this call
    /// just groups PRs that already form a valid chain; it does not create or
    /// modify any PR itself. GitLab has no equivalent concept: returns
    /// `Ok(None)` as a documented no-op rather than an error. Logs the
    /// outbound call (start/done/error) via `rlog!`.
    pub fn create_stack(&self, pr_numbers: &[i64]) -> Result<Option<CreatedStack>, String> {
        if self.kind != ForgeKind::GitHub {
            return Ok(None);
        }
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [forge] create stack start kind={} repo={} pull_requests={pr_numbers:?}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.create_stack_inner(pr_numbers);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(stack) => crate::rlog!(
                INFO,
                "ralphus [forge] create stack done kind={} repo={} number={}",
                self.kind.as_str(),
                self.repo_path,
                stack.number
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] create stack failed kind={} repo={}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result.map(Some)
    }

    fn create_stack_inner(&self, pr_numbers: &[i64]) -> Result<CreatedStack, String> {
        let token = self.require_token()?;
        let url = format!("{}/repos/{}/stacks", self.api_base, self.repo_path);
        let payload = serde_json::json!({ "pull_requests": pr_numbers });
        let resp = self.send(
            ureq::post(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Accept", "application/vnd.github+json"),
            &payload,
        )?;
        let number = resp["number"]
            .as_i64()
            .ok_or_else(|| format!("unexpected GitHub stack response shape: {resp}"))?;
        Ok(CreatedStack { number })
    }

    /// Append newly-created PR numbers (bottom-to-top) onto the top of an
    /// already-registered GitHub PR stack
    /// (`POST /repos/{owner}/{repo}/stacks/{stack_number}/add`) -- the first
    /// of `pr_numbers` must base onto the stack's current top PR's head ref.
    /// GitLab has no equivalent concept: returns `Ok(())` as a documented
    /// no-op rather than an error. Logs the outbound call (start/done/error)
    /// via `rlog!`.
    pub fn add_to_stack(&self, stack_number: i64, pr_numbers: &[i64]) -> Result<(), String> {
        if self.kind != ForgeKind::GitHub {
            return Ok(());
        }
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [forge] add to stack start kind={} repo={} stack={stack_number} pull_requests={pr_numbers:?}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.add_to_stack_inner(stack_number, pr_numbers);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(()) => crate::rlog!(
                INFO,
                "ralphus [forge] add to stack done kind={} repo={} stack={stack_number}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] add to stack failed kind={} repo={} stack={stack_number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn add_to_stack_inner(&self, stack_number: i64, pr_numbers: &[i64]) -> Result<(), String> {
        let token = self.require_token()?;
        let url = format!(
            "{}/repos/{}/stacks/{stack_number}/add",
            self.api_base, self.repo_path
        );
        let payload = serde_json::json!({ "pull_requests": pr_numbers });
        self.send(
            ureq::post(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Accept", "application/vnd.github+json"),
            &payload,
        )?;
        Ok(())
    }

    /// Whether a GitHub-native PR stack still exists on the forge.
    ///
    /// A recorded stack can disappear when it is dissolved outside ralphus or
    /// when a successful `unstack` call is followed by a local interruption.
    /// `404` is therefore a normal negative result rather than an API error.
    pub fn stack_exists(&self, stack_number: i64) -> Result<bool, String> {
        if self.kind != ForgeKind::GitHub {
            return Ok(false);
        }
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its Store-owning caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] get stack start kind={} repo={} stack={stack_number}",
            self.kind.as_str(),
            self.repo_path
        );
        let token = self.require_token()?;
        let url = format!(
            "{}/repos/{}/stacks/{stack_number}",
            self.api_base, self.repo_path
        );
        let result = match ureq::get(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/vnd.github+json")
            .call()
        {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(404, _)) => Ok(false),
            Err(e) => Err(self.describe_evicting(e).into()),
        };
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(exists) => crate::rlog!(
                DEBUG,
                "ralphus [forge] get stack done kind={} repo={} stack={stack_number} exists={exists}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                WARNING,
                "ralphus [forge] get stack failed kind={} repo={} stack={stack_number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    /// Return the ordered PR numbers in a GitHub-native stack. `None` means
    /// the stack was removed from the forge. GitLab has no native stack
    /// object, so it also returns `Ok(None)` without making a request.
    pub fn get_stack_pull_requests(&self, stack_number: i64) -> Result<Option<Vec<i64>>, String> {
        if self.kind != ForgeKind::GitHub {
            return Ok(None);
        }
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] get stack members start kind={} repo={} stack={stack_number}",
            self.kind.as_str(),
            self.repo_path
        );
        let token = self.require_token()?;
        let url = format!(
            "{}/repos/{}/stacks/{stack_number}",
            self.api_base, self.repo_path
        );
        let result = match ureq::get(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/vnd.github+json")
            .call()
        {
            Ok(response) => {
                let body = response
                    .into_string()
                    .map_err(|e| format!("could not read GitHub stack response: {e}"))?;
                let value: serde_json::Value = serde_json::from_str(&body)
                    .map_err(|e| format!("could not parse GitHub stack response: {e}"))?;
                let members = value["pull_requests"]
                    .as_array()
                    .ok_or_else(|| "GitHub stack response missing pull_requests".to_string())?
                    .iter()
                    .map(|pr| {
                        pr["number"]
                            .as_i64()
                            .ok_or_else(|| "GitHub stack member missing number".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Some(members))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(self.describe_evicting(e).into()),
        };
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(Some(members)) => crate::rlog!(
                DEBUG,
                "ralphus [forge] get stack members done kind={} repo={} stack={stack_number} pull_requests={members:?}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(None) => crate::rlog!(
                DEBUG,
                "ralphus [forge] get stack members done kind={} repo={} stack={stack_number} missing",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                WARNING,
                "ralphus [forge] get stack members failed kind={} repo={} stack={stack_number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    /// Remove every unmerged PR from a registered GitHub stack
    /// (`POST /repos/{owner}/{repo}/stacks/{stack_number}/unstack`), which
    /// dissolves the stack once nothing is left in it. The PRs themselves are
    /// untouched — they keep their numbers, comments and base refs, and only
    /// stop being displayed as a linked stack.
    ///
    /// GitHub refuses to change the base ref of a PR while it belongs to a
    /// stack, so repointing a stack at a different base branch means dissolving
    /// it, moving the bases, and registering it again with [`create_stack`] —
    /// see [`crate::pr::resync_pr_bases`]. GitLab has no equivalent concept:
    /// returns `Ok(())` as a documented no-op rather than an error. Logs the
    /// outbound call (start/done/error) via `rlog!`.
    pub fn unstack(&self, stack_number: i64) -> Result<(), String> {
        if self.kind != ForgeKind::GitHub {
            return Ok(());
        }
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            INFO,
            "ralphus [forge] unstack start kind={} repo={} stack={stack_number}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.unstack_inner(stack_number);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(()) => crate::rlog!(
                INFO,
                "ralphus [forge] unstack done kind={} repo={} stack={stack_number}",
                self.kind.as_str(),
                self.repo_path
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] unstack failed kind={} repo={} stack={stack_number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn unstack_inner(&self, stack_number: i64) -> Result<(), String> {
        let token = self.require_token()?;
        let url = format!(
            "{}/repos/{}/stacks/{stack_number}/unstack",
            self.api_base, self.repo_path
        );
        ureq::post(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/vnd.github+json")
            .set("Content-Type", "application/json")
            .send_string("{}")
            .map_err(|e| self.describe_evicting(e))?;
        Ok(())
    }

    /// List human-authored comments on a PR/MR, oldest first. GitLab's
    /// system-generated notes (label changes, etc.) are filtered out since
    /// they are never actionable feedback. Logs the outbound call
    /// (start/done/error) via `rlog!`.
    pub fn list_pr_comments(&self, number: i64) -> Result<Vec<PrComment>, String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] list pr comments start kind={} repo={} number={number}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.list_pr_comments_inner(number);
        match &result {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Ok(comments) => crate::rlog!(
                DEBUG,
                "ralphus [forge] list pr comments done kind={} repo={} number={number} count={}",
                self.kind.as_str(),
                self.repo_path,
                comments.len()
            ),
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            Err(e) => crate::rlog!(
                ERROR,
                "ralphus [forge] list pr comments failed kind={} repo={} number={number}: {e}",
                self.kind.as_str(),
                self.repo_path
            ),
        }
        result
    }

    fn list_pr_comments_inner(&self, number: i64) -> Result<Vec<PrComment>, String> {
        let token = self.require_token()?;
        match self.kind {
            ForgeKind::GitHub => {
                // GitHub splits PR feedback across two resources: general
                // conversation (a PR is an issue) and inline review comments
                // on the diff. Both are read, because this is what feeds
                // `pull-feedback`, and a reviewer's line comments are usually
                // the substantive half of a review.
                //
                // Reading only the conversation endpoint meant inline comments
                // could never be claimed into `guardian_pr_feedback_actioned`,
                // so they were never actioned into the worktree and the
                // un-actioned count could never reach zero for any PR that had
                // one.
                let mut out = Vec::new();
                for path in [
                    format!("issues/{number}/comments"),
                    format!("pulls/{number}/comments"),
                ] {
                    let url = format!(
                        "{}/repos/{}/{path}?per_page={PER_PAGE}",
                        self.api_base, self.repo_path
                    );
                    let resp = self.get(
                        ureq::get(&url)
                            .set("Authorization", &format!("Bearer {token}"))
                            .set("Accept", "application/vnd.github+json"),
                    )?;
                    let items = resp.as_array().ok_or_else(|| {
                        format!("unexpected GitHub comments response shape: {resp}")
                    })?;
                    out.extend(parse_github_comments(items));
                }
                // Oldest first across both sources, matching this method's
                // documented ordering. Both endpoints emit ISO-8601 UTC, which
                // sorts correctly as text.
                out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                Ok(out)
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{number}/notes?per_page={PER_PAGE}",
                    self.api_base, self.repo_path
                );
                let resp = self.get(ureq::get(&url).set("PRIVATE-TOKEN", token))?;
                let items = resp
                    .as_array()
                    .ok_or_else(|| format!("unexpected GitLab notes response shape: {resp}"))?;
                Ok(parse_gitlab_notes(items))
            }
        }
    }

    /// Conditional counterpart to [`Self::list_pr_comments`] (RAL-366): sends
    /// `If-None-Match: etag` when `etag` is `Some`, and returns
    /// [`CommentsPoll::NotModified`] on a `304` -- no forge quota spent, no
    /// re-parse. `source` selects which of GitHub's two comment endpoints to
    /// poll (conversation vs. inline review comments); GitLab has only one
    /// endpoint covering both kinds, so `source` is ignored there. Returns the
    /// structured [`ForgeError`] rather than a plain string so the RAL-366
    /// cache poller can back off on a 429/403 instead of retrying next cycle.
    pub fn list_pr_comments_conditional(
        &self,
        number: i64,
        source: PrCommentEndpoint,
        etag: Option<&str>,
    ) -> Result<CommentsPoll, ForgeError> {
        let token = self.require_token().map_err(ForgeError::other)?;
        let req = match (self.kind, source) {
            (ForgeKind::GitHub, PrCommentEndpoint::Conversation) => {
                let url = format!(
                    "{}/repos/{}/issues/{number}/comments?per_page={PER_PAGE}",
                    self.api_base, self.repo_path
                );
                ureq::get(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Accept", "application/vnd.github+json")
            }
            (ForgeKind::GitHub, PrCommentEndpoint::Review) => {
                let url = format!(
                    "{}/repos/{}/pulls/{number}/comments?per_page={PER_PAGE}",
                    self.api_base, self.repo_path
                );
                ureq::get(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Accept", "application/vnd.github+json")
            }
            (ForgeKind::GitLab, _) => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{number}/notes?per_page={PER_PAGE}",
                    self.api_base, self.repo_path
                );
                ureq::get(&url).set("PRIVATE-TOKEN", token)
            }
        };
        match self.get_conditional(req, etag)? {
            ConditionalGet::NotModified => Ok(CommentsPoll::NotModified),
            ConditionalGet::Modified { value, etag } => {
                let items = value.as_array().ok_or_else(|| {
                    ForgeError::other(format!("unexpected comments response shape: {value}"))
                })?;
                let comments = match self.kind {
                    ForgeKind::GitHub => parse_github_comments(items),
                    ForgeKind::GitLab => parse_gitlab_notes(items),
                };
                Ok(CommentsPoll::Modified { comments, etag })
            }
        }
    }

    /// Best-effort fetch of the repo's PR/MR template, so generated PR bodies
    /// can conform to it. `None` when no template exists or the lookup fails —
    /// callers should fall back to a plain generated body rather than erroring.
    /// Logs whether a template was found via `rlog!` (DEBUG — this path is
    /// best-effort and failing to find a template is not itself an error).
    #[must_use]
    pub fn fetch_pr_template(&self) -> Option<String> {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] fetch pr template start kind={} repo={}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.fetch_pr_template_inner();
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] fetch pr template {} kind={} repo={}",
            if result.is_some() {
                "found"
            } else {
                "not found"
            },
            self.kind.as_str(),
            self.repo_path
        );
        result
    }

    fn fetch_pr_template_inner(&self) -> Option<String> {
        let token = self.token.as_deref();
        match self.kind {
            ForgeKind::GitHub => {
                const CANDIDATES: &[&str] = &[
                    ".github/PULL_REQUEST_TEMPLATE.md",
                    ".github/pull_request_template.md",
                    "PULL_REQUEST_TEMPLATE.md",
                    "docs/PULL_REQUEST_TEMPLATE.md",
                ];
                CANDIDATES
                    .iter()
                    .find_map(|path| self.github_contents(path, token))
            }
            ForgeKind::GitLab => {
                let default_branch = self.gitlab_default_branch(token)?;
                const CANDIDATES: &[&str] = &[
                    ".gitlab/merge_request_templates/Default.md",
                    ".gitlab/merge_request_templates/default.md",
                ];
                CANDIDATES
                    .iter()
                    .find_map(|path| self.gitlab_raw_file(path, &default_branch, token))
            }
        }
    }

    fn github_contents(&self, path: &str, token: Option<&str>) -> Option<String> {
        let url = format!("{}/repos/{}/contents/{path}", self.api_base, self.repo_path);
        let mut req = ureq::get(&url).set("Accept", "application/vnd.github+json");
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let resp = self.get(req).ok()?;
        let b64 = resp["content"].as_str()?;
        decode_base64_maybe_wrapped(b64)
    }

    fn gitlab_default_branch(&self, token: Option<&str>) -> Option<String> {
        let url = format!("{}/projects/{}", self.api_base, self.repo_path);
        let mut req = ureq::get(&url);
        if let Some(t) = token {
            req = req.set("PRIVATE-TOKEN", t);
        }
        let resp = self.get(req).ok()?;
        resp["default_branch"].as_str().map(str::to_string)
    }

    fn gitlab_raw_file(&self, path: &str, ref_: &str, token: Option<&str>) -> Option<String> {
        let encoded_path = path.replace('/', "%2F");
        let url = format!(
            "{}/projects/{}/repository/files/{encoded_path}/raw?ref={ref_}",
            self.api_base, self.repo_path
        );
        let mut req = ureq::get(&url);
        if let Some(t) = token {
            req = req.set("PRIVATE-TOKEN", t);
        }
        req.call().ok()?.into_string().ok()
    }
}

/// Parse a GitHub comments-array response (shared by both
/// `/issues/{n}/comments` and `/pulls/{n}/comments` -- their item shape is
/// identical) into normalised [`PrComment`]s.
fn parse_github_comments(items: &[serde_json::Value]) -> Vec<PrComment> {
    items
        .iter()
        .map(|c| PrComment {
            external_id: c["id"].as_i64().unwrap_or_default().to_string(),
            author: c["user"]["login"].as_str().unwrap_or_default().to_string(),
            body: c["body"].as_str().unwrap_or_default().to_string(),
            created_at: c["created_at"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// Parse a GitLab MR notes-array response into normalised [`PrComment`]s,
/// filtering out system-generated notes (label changes, etc.) -- never
/// actionable feedback.
fn parse_gitlab_notes(items: &[serde_json::Value]) -> Vec<PrComment> {
    items
        .iter()
        .filter(|n| !n["system"].as_bool().unwrap_or(false))
        .map(|n| PrComment {
            external_id: n["id"].as_i64().unwrap_or_default().to_string(),
            author: n["author"]["username"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            body: n["body"].as_str().unwrap_or_default().to_string(),
            created_at: n["created_at"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// GitHub's contents API returns base64 with embedded newlines every 60 chars;
/// strip whitespace before decoding.
fn decode_base64_maybe_wrapped(s: &str) -> Option<String> {
    use base64::Engine as _;
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .ok()?;
    String::from_utf8(bytes).ok()
}

impl ForgeClient {
    /// `POST`/`PUT`/`PATCH` a JSON payload, parsing the JSON response body.
    /// Every mutating forge call in this file routes through this (see the
    /// module-level `impl ForgeClient` blocks above) rather than calling
    /// `ureq` directly, so a 401/403 anywhere always reaches
    /// [`Self::describe_evicting`].
    fn send(
        &self,
        req: ureq::Request,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let resp = req
            .set("Content-Type", "application/json")
            .send_string(&payload.to_string())
            .map_err(|e| self.describe_evicting(e))?;
        parse_body(resp)
    }

    /// `GET`, parsing the JSON response body. See [`Self::send`]'s doc for
    /// why every read call in this file routes through this.
    fn get(&self, req: ureq::Request) -> Result<serde_json::Value, String> {
        let resp = req.call().map_err(|e| self.describe_evicting(e))?;
        parse_body(resp)
    }

    /// `GET` with conditional-request support (RAL-366): sets `If-None-Match`
    /// when `etag` is given, and reports a `304` as
    /// [`ConditionalGet::NotModified`] rather than an error. `304` has no
    /// `Location` header for ureq's redirect-following to act on, so ureq
    /// returns it as `Ok(resp)` (status < 400) rather than `Err`, unlike
    /// every other non-2xx status this file otherwise routes through
    /// [`Self::describe_evicting`] -- checked via `resp.status()` before
    /// attempting to parse what is, on a 304, always an empty body.
    /// Returns the raw [`ForgeError`] (not stringified) so a caller polling
    /// on a schedule can inspect `status`/`retry_after` and back off on a
    /// rate limit instead of retrying next cycle as if nothing happened.
    fn get_conditional(
        &self,
        mut req: ureq::Request,
        etag: Option<&str>,
    ) -> Result<ConditionalGet, ForgeError> {
        if let Some(etag) = etag {
            req = req.set("If-None-Match", etag);
        }
        match req.call() {
            Ok(resp) if resp.status() == 304 => Ok(ConditionalGet::NotModified),
            Ok(resp) => {
                let etag = resp.header("ETag").map(str::to_string);
                let body = resp
                    .into_string()
                    .map_err(|e| ForgeError::other(format!("forge API read: {e}")))?;
                let value = serde_json::from_str(&body)
                    .map_err(|e| ForgeError::other(format!("forge API JSON parse: {e}")))?;
                Ok(ConditionalGet::Modified { value, etag })
            }
            // Defensive: not observed with this ureq version's redirect
            // handling (a 304 without `Location` comes back `Ok` above), but
            // handled in case that behavior ever changes.
            Err(ureq::Error::Status(304, _)) => Ok(ConditionalGet::NotModified),
            Err(e) => Err(self.describe_evicting(e)),
        }
    }

    /// [`describe_error`], plus (RAL-<new>) evicting this client's
    /// [`CLI_TOKEN_CACHE`] entry on a 401/403 -- the daemon's one signal that
    /// a token might genuinely be bad (revoked, expired), as opposed to the
    /// cache's blind TTL. Without this, a token revoked mid-TTL keeps getting
    /// served from cache to every request that hits it until the TTL expires
    /// on its own; with it, the very next resolve re-checks the CLI live
    /// instead of waiting.
    fn describe_evicting(&self, e: ureq::Error) -> ForgeError {
        if let ureq::Error::Status(401 | 403, _) = &e {
            if let Some(host) = &self.cli_token_host {
                evict_cli_token(self.kind, host);
            }
        }
        describe_error(e)
    }
}

/// A forge API error (RAL-366): carries the HTTP status and `Retry-After`
/// header when the forge sent one, alongside the same human-readable message
/// every pre-existing caller already treats as an opaque `String` (via
/// `From<ForgeError> for String`, so every `Result<_, String>` method in this
/// file that reaches this type through `?` keeps compiling unchanged). Only
/// the new RAL-366 cache-poller call sites -- [`ForgeClient::get_conditional`]
/// and [`ForgeClient::list_pr_comments_conditional`] -- need the structured
/// fields, to back off on a rate limit instead of retrying immediately.
#[derive(Debug, Clone)]
pub struct ForgeError {
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
    message: String,
}

impl ForgeError {
    fn other(message: impl Into<String>) -> Self {
        Self {
            status: None,
            retry_after: None,
            message: message.into(),
        }
    }

    /// Whether this is a forge rate-limit/quota response (RAL-366) the
    /// cache poller should back off from rather than retry next cycle as if
    /// nothing happened. GitHub uses 403 for both a genuine auth failure and
    /// secondary rate limiting; there is no way to tell them apart from the
    /// status code alone, but backing off either way is the safe choice --
    /// retrying an auth failure on every cycle is exactly the "retry storm"
    /// risk this exists to avoid.
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(self.status, Some(403 | 429))
    }
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<ForgeError> for String {
    fn from(e: ForgeError) -> String {
        e.message
    }
}

/// Parse a `Retry-After` header value (RAL-366): supports only the
/// delay-seconds form (`Retry-After: 120`), which is what GitHub/GitLab both
/// send on a rate limit -- the HTTP-date form is valid per spec but not
/// something either forge actually emits, so it's left as `None` rather than
/// pulling in a date-parsing dependency for a case that doesn't occur.
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

fn describe_error(e: ureq::Error) -> ForgeError {
    match e {
        ureq::Error::Status(code, resp) => {
            let retry_after = resp.header("Retry-After").and_then(parse_retry_after);
            let body = resp.into_string().unwrap_or_default();
            ForgeError {
                status: Some(code),
                retry_after,
                message: format!("forge API {code}: {body}"),
            }
        }
        other => ForgeError::other(format!("forge API: {other}")),
    }
}

fn parse_body(resp: ureq::Response) -> Result<serde_json::Value, String> {
    let body = resp
        .into_string()
        .map_err(|e| format!("forge API read: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("forge API JSON parse: {e}"))
}

/// Parse a git remote URL into `(host, path)`, where `path` has no leading
/// slash, trailing slash, or `.git` suffix. Supports the three shapes git
/// itself accepts: `git@host:owner/repo.git`, `https://host/owner/repo.git`,
/// and `ssh://git@host/owner/repo.git`.
///
/// `pub(crate)` (RAL-338) so `project_forks`/`pr.rs` can reuse it to derive a
/// fork registration's owner without re-deriving this parsing a second time.
pub(crate) fn parse_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (host, path) = if let Some(rest) = url
        .strip_prefix("ssh://")
        .or_else(|| url.strip_prefix("git://"))
    {
        let rest = rest.split('@').next_back().unwrap_or(rest);
        let mut parts = rest.splitn(2, '/');
        (parts.next()?, parts.next()?)
    } else if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        let rest = rest.split('@').next_back().unwrap_or(rest);
        let mut parts = rest.splitn(2, '/');
        (parts.next()?, parts.next()?)
    } else {
        let (rest, idx) = url.strip_prefix("git@").and_then(|r| {
            let idx = r.find(':')?;
            Some((r, idx))
        })?;
        (&rest[..idx], &rest[idx + 1..])
    };
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some((host.to_string(), path.to_string()))
}

/// Best-effort GitHub owner/org login parsed from a fork's remote URL
/// (RAL-338), for the fork registration surface's optional `fork_owner`
/// auto-fill. GitLab addresses cross-project MRs by numeric project id, not
/// owner, so callers should only use this when the fork's forge is GitHub --
/// a GitLab fork's `fork_owner` is meant to stay `""`.
#[must_use]
pub(crate) fn derive_owner_from_url(url: &str) -> Option<String> {
    let (_, path) = parse_remote_url(url)?;
    let owner = path.split('/').next()?;
    (!owner.is_empty()).then(|| owner.to_string())
}

/// Fallback token resolution when `[forge].token_env` (or its default,
/// `RALPHUS_GITHUB_TOKEN`/`RALPHUS_GITLAB_TOKEN`) isn't set in the daemon's
/// own environment: ask the forge's own CLI for a token it already has
/// cached from a prior interactive `gh auth login` / `glab auth login` on
/// this machine. Best-effort only — the caller (`resolve_remote_for_inner`)
/// treats an `Err` here exactly like "no token configured" and proceeds
/// unauthenticated, but logs the `Err` reason as a warning first, since a
/// spawn failure (e.g. the CLI missing from the daemon's own PATH, which can
/// differ from an interactive shell's) would otherwise look identical to "no
/// CLI installed, working as intended" right up until an unauthenticated
/// request 404s against a private repo with no clue why. Only usable when
/// the daemon process runs on the same machine as that CLI login; a
/// remote/CI daemon still needs the env var.
///
/// TODO: Replace with real user-service authentication once RAL-245 is complete.
fn resolve_cli_token(kind: ForgeKind, host: &str) -> Result<String, String> {
    match kind {
        ForgeKind::GitHub => {
            let mut cmd = std::process::Command::new("gh");
            cmd.arg("auth").arg("token");
            if !host.eq_ignore_ascii_case("github.com") {
                cmd.arg("--hostname").arg(host);
            }
            let output = cmd.output().map_err(|e| {
                format!("could not run `gh auth token` (is `gh` on the daemon's PATH?): {e}")
            })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!(
                    "`gh auth token` exited with {}: {}",
                    output.status,
                    stderr.trim()
                ));
            }
            let token = String::from_utf8(output.stdout)
                .map_err(|e| format!("`gh auth token` printed non-UTF-8 output: {e}"))?;
            let token = token.trim();
            if token.is_empty() {
                Err("`gh auth token` printed an empty token".to_string())
            } else {
                Ok(token.to_string())
            }
        }
        ForgeKind::GitLab => {
            // glab has no single-purpose "print the token" command like `gh
            // auth token`; `auth status --show-token` is the closest thing,
            // and it writes to stderr.
            let output = std::process::Command::new("glab")
                .arg("auth")
                .arg("status")
                .arg("--hostname")
                .arg(host)
                .arg("--show-token")
                .output()
                .map_err(|e| {
                    format!(
                        "could not run `glab auth status` (is `glab` on the daemon's PATH?): {e}"
                    )
                })?;
            let combined = [output.stdout, output.stderr].concat();
            let text = String::from_utf8(combined)
                .map_err(|e| format!("`glab auth status` printed non-UTF-8 output: {e}"))?;
            extract_glab_token(&text).ok_or_else(|| {
                format!(
                    "could not find a token in `glab auth status --show-token` output: {}",
                    text.trim()
                )
            })
        }
    }
}

/// TTL for the [`CLI_TOKEN_CACHE`] entries [`resolve_cli_token_cached`] fills.
///
/// `resolve_remote_for_inner` runs on every `ForgeClient` build, which
/// happens on every PR-sync-status check and every PR list refresh — with no
/// cache, an unset token env var meant the daemon shelled out to `gh`/`glab`
/// on nearly every board poll. Long enough to make that cost negligible;
/// short enough that a token rotated by re-running `gh auth login` /
/// `glab auth login` on this machine is picked up again within a few
/// minutes rather than requiring a daemon restart.
const CLI_TOKEN_TTL: Duration = Duration::from_secs(300);

/// `(kind, host) -> (token, fetched_at)` map backing [`CLI_TOKEN_CACHE`].
type CliTokenCache = HashMap<(ForgeKind, String), (String, Instant)>;

/// Cache of [`resolve_cli_token`] results, keyed by `(kind, host)`. Only
/// successes are cached — a failure (CLI missing, not logged in) is cheap to
/// retry and re-checking it live means a login performed after the daemon
/// started is picked up on the very next call instead of waiting out a TTL.
static CLI_TOKEN_CACHE: Mutex<Option<CliTokenCache>> = Mutex::new(None);

/// [`resolve_cli_token`], cached for [`CLI_TOKEN_TTL`] per `(kind, host)`.
fn resolve_cli_token_cached(kind: ForgeKind, host: &str) -> Result<String, String> {
    let key = (kind, host.to_string());
    {
        let cache = CLI_TOKEN_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((token, fetched_at)) = cache.as_ref().and_then(|m| m.get(&key)) {
            if fetched_at.elapsed() < CLI_TOKEN_TTL {
                return Ok(token.clone());
            }
        }
    }
    let token = resolve_cli_token(kind, host)?;
    let mut cache = CLI_TOKEN_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .get_or_insert_with(HashMap::new)
        .insert(key, (token.clone(), Instant::now()));
    Ok(token)
}

/// Drops the cached CLI-fallback token for `(kind, host)`, if any, so the
/// next [`resolve_cli_token_cached`] call re-checks the CLI live instead of
/// serving a possibly-revoked token for the rest of [`CLI_TOKEN_TTL`].
/// Called by [`ForgeClient::describe_evicting`] on a 401/403. A no-op when
/// nothing is cached for that key (e.g. the token came from an env var).
fn evict_cli_token(kind: ForgeKind, host: &str) {
    let mut cache = CLI_TOKEN_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(map) = cache.as_mut() {
        if map.remove(&(kind, host.to_string())).is_some() {
            // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
            crate::rlog!(
                WARNING,
                "ralphus [forge] evicted cached {} token for host={host} after a 401/403 -- \
                 will re-check the CLI on the next request",
                kind.as_str()
            );
        }
    }
}

/// Extracts the token from `glab auth status --show-token`'s combined
/// stdout+stderr text. The line carrying it varies by how the token is
/// stored: a plain "Token: <value>" line for a config-file-stored token, but
/// "✓ Token found in operating system keyring: <value>" when it's in the OS
/// keyring (the default on Windows/macOS) -- so this looks for any line
/// mentioning "Token" and takes whatever follows its *last* colon, rather
/// than anchoring on one exact prefix (a Windows-keyring login previously
/// fell through to "no token configured" because the "Token:" prefix match
/// never fired on that line's actual wording).
fn extract_glab_token(text: &str) -> Option<String> {
    let token = text.lines().find_map(|line| {
        if !line.contains("Token") {
            return None;
        }
        line.rsplit_once(':')
            .map(|(_, rest)| rest.trim().to_string())
    })?;
    (!token.is_empty()).then_some(token)
}

/// Resolve which git remote a review's own `base_branch` designates (RAL-190,
/// extended by RAL-282 to also honor a bare branch's own `@{u}` upstream) —
/// the single shared precedence used both to pick which forge host a PR is
/// submitted against ([`resolve_remote`]) and which remote PR-branch
/// pushes/pulls target (`pr.rs`):
///
/// 1. `base_branch` in `<remote>/<branch>` form, where `<remote>` names a
///    real configured remote (e.g. `"origin/main"`, `"alt/beta"`) -> that
///    remote.
/// 2. Else `base_branch` names a bare local branch with its own `@{u}`
///    upstream tracking configured -> that upstream's remote.
/// 3. `[forge].remote` config, if set.
/// 4. `"origin"`.
///
/// `[forge].remote` never wins over what `base_branch` itself resolves to —
/// it is a fallback only, consulted when neither of the above resolves.
#[must_use]
pub(crate) fn resolve_remote_name(root: &Path, base_branch: &str, cfg: &ForgeConfig) -> String {
    resolve_remote_name_excluding(root, base_branch, cfg, None)
}

/// Like [`resolve_remote_name`], but steps 1-2 never resolve to `exclude`
/// (RAL-338): used when computing the *parent's* remote for a fork-mode
/// review, so a stray `@{upstream}` or `<remote>/<branch>`-form `base_branch`
/// that happens to point at the project's registered fork remote can't be
/// mistaken for the parent's remote. Every existing (non-fork) call site
/// keeps calling [`resolve_remote_name`] unchanged, which passes `None` here
/// and is therefore byte-identical to before this exclusion existed.
#[must_use]
pub(crate) fn resolve_remote_name_excluding(
    root: &Path,
    base_branch: &str,
    cfg: &ForgeConfig,
    exclude: Option<&str>,
) -> String {
    remote_from_base_branch(root, base_branch)
        .filter(|r| Some(r.as_str()) != exclude)
        .or_else(|| {
            remote_from_branch_upstream(root, base_branch).filter(|r| Some(r.as_str()) != exclude)
        })
        .unwrap_or_else(|| default_remote_name(cfg))
}

/// A resolved routing target for one fork-aware PR/MR operation (RAL-338):
/// which client to call, what `head`/`base` the forge call itself needs, the
/// GitLab-only `target_project_id`, and which repository label the
/// created/queried PR is filed under (`PullRequestView.repo`). See
/// `pr.rs`'s fork-aware stack route calculation for how this is built, and
/// this module's doc comment's Risks-derived asymmetry: GitLab always calls
/// the fork's client (setting `target_project_id` only for the cross-project
/// root); GitHub calls the *parent's* client for the cross-repo root and the
/// fork's client for everything else.
#[derive(Clone)]
pub struct PrRoute {
    pub client: ForgeClient,
    pub head: String,
    pub base: String,
    pub target_project_id: Option<i64>,
    /// The repository label the PR/MR is filed under. GitHub: the parent's
    /// `owner/repo` for the cross-repo root, else the fork's `owner/repo`.
    /// GitLab: always the fork's encoded path, since a GitLab MR's `iid` is
    /// scoped to whichever project it was created on, never the
    /// `target_project_id`.
    pub repo: String,
}

impl PrRoute {
    /// Create the PR/MR this route describes.
    ///
    /// # Errors
    /// Propagates the underlying forge API failure.
    pub fn create_pull_request(&self, title: &str, body: &str) -> Result<CreatedPr, String> {
        self.client.create_pull_request_routed(
            title,
            body,
            &self.head,
            &self.base,
            self.target_project_id,
        )
    }

    /// Look up whether this route's exact head already has an open PR/MR --
    /// see [`ForgeClient::find_open_pull_request`] for why this is a
    /// structured query rather than a creation-error-text check. Call this
    /// before [`Self::create_pull_request`] and adopt what it finds instead
    /// of creating a duplicate.
    ///
    /// # Errors
    /// Propagates the underlying forge API failure.
    pub fn find_existing_pull_request(&self) -> Result<Option<ExistingPr>, String> {
        self.client.find_open_pull_request(&self.head)
    }
}

/// Pick whichever candidate client's repo label matches `repo` (RAL-338) --
/// the reconstruction step per-PR operations need once a review has a
/// registered fork, since a PR's stored `repo` column may name either the
/// parent or the fork, and the two clients already resolved for a fork-mode
/// review are the only two candidates that could ever apply. Returns `None`
/// if neither matches (e.g. `repo` was recorded against a since-changed
/// remote).
#[must_use]
pub(crate) fn client_for_repo<'a>(
    repo: &str,
    candidates: &[&'a ForgeClient],
) -> Option<&'a ForgeClient> {
    candidates.iter().find(|c| c.repo_label() == repo).copied()
}

/// One project's resolved fork-network membership, as returned by a live
/// `lookup_fork_network` call. Kept separate from the HTTP call itself so
/// [`classify_fork_relationship`] stays pure and offline-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkNetworkInfo {
    /// This project's own repo label.
    pub label: String,
    /// The label of the project this one is directly forked from, if any.
    pub forked_from: Option<String>,
    /// The ultimate fork-network root label: walks `forked_from` to its end;
    /// equal to `label` itself when this project isn't a fork of anything.
    pub network_root: String,
}

/// The result of a live fork-network lookup for one project (RAL-338). A
/// `404`/`403` is `NotVisible`, not an error -- see
/// [`classify_fork_relationship`]'s doc comment for why that distinction
/// matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkLookup {
    Found(ForkNetworkInfo),
    NotVisible,
}

/// How a `fork` project relates to its intended `parent`'s fork network
/// (RAL-338), used as the required pre-flight before ever filing a
/// cross-repository PR/MR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkRelationship {
    /// `fork` is directly forked from `parent`.
    SameNetwork,
    /// `fork` and `parent` share the same ultimate fork-network root, but
    /// not directly -- a fork of a fork, or siblings. Not blocked, but
    /// worth a warning: cross-repository behavior (promotion, base updates)
    /// is only proven for a direct relationship.
    SameNetworkIndirect,
    /// Both projects are forge-visible, but share no fork network at all.
    NoRelationship,
    /// The fork or parent project could not be read (403/404). This is NOT
    /// proof of no relationship -- a private fork the token can't see looks
    /// identical to a nonexistent one over these APIs, so callers must not
    /// treat this the same as [`Self::NoRelationship`].
    NotVisible,
    /// The two repositories live on different forge hosts/kinds. There is no
    /// cross-repository PR path regardless of any real relationship.
    CrossInstance,
}

impl ForkRelationship {
    /// Whether a submit should be hard-blocked without
    /// `--allow-unlinked-fork` -- only a definite absence of any
    /// relationship. Every other outcome (indirect, not visible,
    /// cross-instance) is either allowed-with-a-warning or already fatal for
    /// a different, more specific reason the caller reports separately.
    #[must_use]
    pub fn blocks_submission(self) -> bool {
        matches!(self, Self::NoRelationship)
    }
}

/// Classify how `fork` relates to `parent`'s fork network from each side's
/// already-resolved [`NetworkLookup`] (RAL-338). Pure and offline-testable --
/// see `ForgeClient::lookup_fork_network` for the live HTTP lookups that
/// produce a real `NetworkLookup`.
///
/// Guards `fork_kind != parent_kind` first (different forge instances have no
/// cross-repository path at all, independent of whether either lookup even
/// succeeded), then treats either side being unreadable as [`ForkRelationship::NotVisible`]
/// rather than [`ForkRelationship::NoRelationship`] -- a 404/403 is never
/// treated as proof of anything.
#[must_use]
pub fn classify_fork_relationship(
    fork_kind: ForgeKind,
    parent_kind: ForgeKind,
    fork: &NetworkLookup,
    parent: &NetworkLookup,
) -> ForkRelationship {
    if fork_kind != parent_kind {
        return ForkRelationship::CrossInstance;
    }
    let (NetworkLookup::Found(f), NetworkLookup::Found(p)) = (fork, parent) else {
        return ForkRelationship::NotVisible;
    };
    if f.network_root != p.network_root {
        return ForkRelationship::NoRelationship;
    }
    if f.forked_from.as_deref() == Some(p.label.as_str()) {
        ForkRelationship::SameNetwork
    } else {
        ForkRelationship::SameNetworkIndirect
    }
}

impl ForgeClient {
    /// Live fork-network lookup for [`classify_fork_relationship`] (RAL-338).
    /// A `404`/`403` maps to [`NetworkLookup::NotVisible`] rather than
    /// `Err`, since "can't read it" and "doesn't exist" are indistinguishable
    /// over these APIs and both must be treated as "not proof of no
    /// relationship" by the caller. Any other failure (network error,
    /// missing token, unexpected shape) also degrades to `NotVisible` for
    /// the same reason -- a relationship pre-flight that can't complete
    /// must never silently read as "confirmed unrelated".
    ///
    /// GitLab's `forked_from_project` only names the *direct* parent, so
    /// this walks it (bounded to 10 hops, matching the deepest ordinary fork
    /// chain anyone would plausibly hit) to find the network root. GitHub's
    /// `source` field already names the ultimate root directly.
    #[must_use]
    pub fn lookup_fork_network(&self) -> NetworkLookup {
        match self.kind {
            ForgeKind::GitHub => self.lookup_fork_network_github(),
            ForgeKind::GitLab => self.lookup_fork_network_gitlab(),
        }
    }

    fn lookup_fork_network_github(&self) -> NetworkLookup {
        let Ok(token) = self.require_token() else {
            return NetworkLookup::NotVisible;
        };
        let url = format!("{}/repos/{}", self.api_base, self.repo_path);
        let Ok(resp) = self.get(
            ureq::get(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Accept", "application/vnd.github+json"),
        ) else {
            return NetworkLookup::NotVisible;
        };
        let Some(label) = resp["full_name"].as_str() else {
            return NetworkLookup::NotVisible;
        };
        let is_fork = resp["fork"].as_bool().unwrap_or(false);
        let forked_from = is_fork
            .then(|| resp["parent"]["full_name"].as_str())
            .flatten()
            .map(str::to_string);
        let network_root = if is_fork {
            resp["source"]["full_name"]
                .as_str()
                .unwrap_or(label)
                .to_string()
        } else {
            label.to_string()
        };
        NetworkLookup::Found(ForkNetworkInfo {
            label: label.to_string(),
            forked_from,
            network_root,
        })
    }

    fn lookup_fork_network_gitlab(&self) -> NetworkLookup {
        let Ok(token) = self.require_token() else {
            return NetworkLookup::NotVisible;
        };
        let Some(mut resp) = self.get_gitlab_project(token, &self.repo_path) else {
            return NetworkLookup::NotVisible;
        };
        let Some(label) = resp["path_with_namespace"].as_str().map(str::to_string) else {
            return NetworkLookup::NotVisible;
        };
        let forked_from = resp["forked_from_project"]["path_with_namespace"]
            .as_str()
            .map(str::to_string);
        let mut network_root = label.clone();
        let mut hops = 0;
        while let Some(parent_id) = resp["forked_from_project"]["id"].as_i64() {
            hops += 1;
            if hops > 10 {
                break;
            }
            let Some(parent_resp) = self.get_gitlab_project(token, &parent_id.to_string()) else {
                break;
            };
            let Some(parent_label) = parent_resp["path_with_namespace"].as_str() else {
                break;
            };
            network_root = parent_label.to_string();
            resp = parent_resp;
        }
        NetworkLookup::Found(ForkNetworkInfo {
            label,
            forked_from,
            network_root,
        })
    }

    fn get_gitlab_project(&self, token: &str, path_or_id: &str) -> Option<serde_json::Value> {
        let url = format!("{}/projects/{path_or_id}", self.api_base);
        self.get(ureq::get(&url).set("PRIVATE-TOKEN", token)).ok()
    }
}

/// Steps 3-4 of [`resolve_remote_name`] on their own: the branch-independent
/// fallback, `[forge].remote` else `"origin"`. Exposed separately for the
/// callers that have no review branch to key steps 1-2 off at all (see
/// project provisioning, which uses a project's registered clone URL
/// for remote-machine provisioning), so they share this crate's one definition
/// of the default rather than open-coding `unwrap_or("origin")` again.
#[must_use]
pub(crate) fn default_remote_name(cfg: &ForgeConfig) -> String {
    cfg.remote.clone().unwrap_or_else(|| "origin".to_string())
}

/// Step 1 of [`resolve_remote_name`]: `base_branch` written in
/// `<remote>/<branch>` form and that leading segment names a real configured
/// remote. Returns `None` when `base_branch` has no `/` at all, or its
/// leading segment isn't a real remote (so it's read as a literal branch name
/// that happens to contain a slash, e.g. a personal `colin/main`).
fn remote_from_base_branch(root: &Path, base_branch: &str) -> Option<String> {
    let (candidate, _) = base_branch.split_once('/')?;
    crate::guardian_merge::git(root, &["remote", "get-url", candidate]).ok()?;
    Some(candidate.to_string())
}

/// Step 2 of [`resolve_remote_name`]: `base_branch` read as a bare local
/// branch name, resolved via its own `@{u}` upstream tracking — not the
/// current checkout's `@{u}` (a bare `@{u}` reflects whatever happens to be
/// checked out in the shared guardian repo at call time, not the review's own
/// base branch). Returns `None` when the branch doesn't exist locally or has
/// no upstream configured.
///
/// An empty `base_branch` returns `None` without shelling out: `format!`ing it
/// into the rev-parse argument would leave a bare `@{u}`, i.e. exactly the
/// current-checkout reading this function exists to avoid.
fn remote_from_branch_upstream(root: &Path, base_branch: &str) -> Option<String> {
    if base_branch.trim().is_empty() {
        return None;
    }
    let upstream = crate::guardian_merge::git(
        root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &format!("{base_branch}@{{u}}"),
        ],
    )
    .ok()?;
    let (remote, _branch) = upstream.trim().split_once('/')?;
    Some(remote.to_string())
}

/// Resolve the effective forge client for a git repository at `root`,
/// combining the layered [`ForgeConfig`] (explicit overrides) with
/// autodetection from `git remote get-url <remote>` (host → kind, path →
/// repo). The remote itself is resolved via [`resolve_remote_name`], keyed
/// off the review's own `base_branch` rather than `[forge].remote` directly
/// (RAL-282). Fails only when the remote can't be read/parsed or no forge
/// kind can be determined — a missing token is not an error here (see
/// [`ForgeClient::require_token`]). Logs the resolved kind/repo/api_base (or
/// the failure reason) via `rlog!`.
pub fn resolve_remote(
    root: &Path,
    base_branch: &str,
    cfg: &ForgeConfig,
) -> Result<ForgeClient, String> {
    let remote_name = resolve_remote_name(root, base_branch, cfg);
    resolve_remote_for(root, &remote_name, cfg)
}

/// Like [`resolve_remote`], but takes an already-resolved remote name
/// directly rather than deriving one from a review's `base_branch` (RAL-338).
/// Used when the caller has already picked the remote itself: the parent's
/// fork-excluded remote (see [`resolve_remote_name_excluding`]), or a
/// registered fork's own `remote_name`.
pub fn resolve_remote_for(
    root: &Path,
    remote_name: &str,
    cfg: &ForgeConfig,
) -> Result<ForgeClient, String> {
    let result = resolve_remote_for_inner(root, remote_name, cfg);
    match &result {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        Ok(client) => crate::rlog!(
            DEBUG,
            "ralphus [forge] resolved kind={} repo={} api_base={}",
            client.kind.as_str(),
            client.repo_path,
            client.api_base
        ),
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        Err(e) => crate::rlog!(WARNING, "ralphus [forge] resolve remote failed: {e}"),
    }
    result
}

fn resolve_remote_for_inner(
    root: &Path,
    remote_name: &str,
    cfg: &ForgeConfig,
) -> Result<ForgeClient, String> {
    let url = crate::guardian_merge::git(root, &["remote", "get-url", remote_name])
        .map_err(|e| format!("could not read remote '{remote_name}': {e}"))?;
    let (host, path) = parse_remote_url(url.trim())
        .ok_or_else(|| format!("could not parse remote url: {}", url.trim()))?;

    let kind = cfg
        .kind
        .as_deref()
        .and_then(ForgeKind::parse)
        .or_else(|| ForgeKind::from_host(&host))
        .ok_or_else(|| {
            format!("could not determine forge kind for host '{host}'; set [forge].kind explicitly")
        })?;

    let api_base = cfg
        .api_base
        .clone()
        .unwrap_or_else(|| kind.default_api_base(&host));

    let token_env = cfg
        .token_env
        .clone()
        .unwrap_or_else(|| kind.default_token_env().to_string());
    // Env var wins when set; otherwise fall back to the forge CLI's own
    // cached login (see `resolve_cli_token`'s doc comment for scope/limits).
    // A failed fallback is logged loudly rather than folded silently into
    // "no token" -- a client built with no token still makes unauthenticated
    // requests (see the module doc's "Auth" section), which only 404 much
    // later against a private repo with zero clue as to why.
    // TODO: Replace with real user-service authentication once RAL-245 is complete.
    let token = match std::env::var(&token_env) {
        Ok(t) => Some(t),
        Err(_) => match resolve_cli_token_cached(kind, &host) {
            Ok(t) => {
                // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
                crate::rlog!(
                    DEBUG,
                    "ralphus [forge] resolved token via CLI fallback kind={} host={host}",
                    kind.as_str()
                );
                Some(t)
            }
            Err(reason) => {
                // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
                crate::rlog!(
                    WARNING,
                    "ralphus [forge] no {} token available -- env {token_env} is unset and the CLI \
                     fallback failed: {reason}. Requests will go out unauthenticated and may fail \
                     (e.g. 404) against a private repo.",
                    kind.as_str()
                );
                None
            }
        },
    };

    let repo_path = match kind {
        ForgeKind::GitHub => path,
        ForgeKind::GitLab => path.replace('/', "%2F"),
    };

    Ok(ForgeClient::new(kind, api_base, repo_path, token).with_cli_token_host(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_pr_template_reads_the_github_default_template() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(
                req.url(),
                "/repos/acme/widget/contents/.github/PULL_REQUEST_TEMPLATE.md"
            );
            req.respond(
                tiny_http::Response::from_string(r#"{"content":"IyMgU3VtbWFyeQo="}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );

        assert_eq!(client.fetch_pr_template().as_deref(), Some("## Summary\n"));
        handle.join().unwrap();
    }

    #[test]
    fn fetch_pr_template_reads_the_gitlab_default_template() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/acme%2Fwidget");
            req.respond(
                tiny_http::Response::from_string(r#"{"default_branch":"main"}"#)
                    .with_status_code(200),
            )
            .unwrap();

            let req = server.recv().unwrap();
            assert_eq!(
                req.url(),
                "/projects/acme%2Fwidget/repository/files/.gitlab%2Fmerge_request_templates%2FDefault.md/raw?ref=main"
            );
            req.respond(tiny_http::Response::from_string("## Summary\n").with_status_code(200))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "acme%2Fwidget".to_string(),
            Some("tok".to_string()),
        );

        assert_eq!(client.fetch_pr_template().as_deref(), Some("## Summary\n"));
        handle.join().unwrap();
    }

    #[test]
    fn parses_ssh_shorthand_remote() {
        let (host, path) = parse_remote_url("git@github.com:acme/widget.git").unwrap();
        assert_eq!(host, "github.com");
        assert_eq!(path, "acme/widget");
    }

    #[test]
    fn parses_https_remote() {
        let (host, path) = parse_remote_url("https://github.com/acme/widget.git").unwrap();
        assert_eq!(host, "github.com");
        assert_eq!(path, "acme/widget");
    }

    #[test]
    fn parses_https_remote_without_dot_git() {
        let (host, path) = parse_remote_url("https://gitlab.example.com/group/sub/proj").unwrap();
        assert_eq!(host, "gitlab.example.com");
        assert_eq!(path, "group/sub/proj");
    }

    #[test]
    fn parses_ssh_scheme_remote() {
        let (host, path) = parse_remote_url("ssh://git@gitlab.com/acme/widget.git").unwrap();
        assert_eq!(host, "gitlab.com");
        assert_eq!(path, "acme/widget");
    }

    #[test]
    fn rejects_unparseable_remote() {
        assert!(parse_remote_url("not a url").is_none());
    }

    #[test]
    fn derive_owner_from_url_handles_every_supported_url_form() {
        assert_eq!(
            derive_owner_from_url("git@github.com:alice/proj.git"),
            Some("alice".to_string())
        );
        assert_eq!(
            derive_owner_from_url("https://github.com/alice/proj.git"),
            Some("alice".to_string())
        );
        assert_eq!(
            derive_owner_from_url("https://github.com/alice/proj"),
            Some("alice".to_string())
        );
        assert_eq!(
            derive_owner_from_url("ssh://git@github.com/alice/proj.git"),
            Some("alice".to_string())
        );
        assert_eq!(derive_owner_from_url("not a url"), None);
    }

    #[test]
    fn client_for_repo_picks_the_matching_candidate() {
        let parent = ForgeClient::new(
            ForgeKind::GitHub,
            "https://api.github.com".to_string(),
            "acme/widget".to_string(),
            None,
        );
        let fork = ForgeClient::new(
            ForgeKind::GitHub,
            "https://api.github.com".to_string(),
            "alice/widget".to_string(),
            None,
        );
        let candidates = [&parent, &fork];
        assert_eq!(
            client_for_repo("alice/widget", &candidates)
                .unwrap()
                .repo_label(),
            "alice/widget"
        );
        assert_eq!(
            client_for_repo("acme/widget", &candidates)
                .unwrap()
                .repo_label(),
            "acme/widget"
        );
        assert!(client_for_repo("someone/else", &candidates).is_none());
    }

    fn network(label: &str, forked_from: Option<&str>, root: &str) -> NetworkLookup {
        NetworkLookup::Found(ForkNetworkInfo {
            label: label.to_string(),
            forked_from: forked_from.map(str::to_string),
            network_root: root.to_string(),
        })
    }

    #[test]
    fn classify_fork_relationship_direct_fork() {
        let fork = network("alice/widget", Some("acme/widget"), "acme/widget");
        let parent = network("acme/widget", None, "acme/widget");
        assert_eq!(
            classify_fork_relationship(ForgeKind::GitHub, ForgeKind::GitHub, &fork, &parent),
            ForkRelationship::SameNetwork
        );
        assert!(!ForkRelationship::SameNetwork.blocks_submission());
    }

    #[test]
    fn classify_fork_relationship_indirect_fork_of_a_fork() {
        // `fork` was forked from an intermediate project, not directly from
        // `parent`, but both ultimately trace back to the same root.
        let fork = network("bob/widget", Some("alice/widget"), "acme/widget");
        let parent = network("acme/widget", None, "acme/widget");
        assert_eq!(
            classify_fork_relationship(ForgeKind::GitHub, ForgeKind::GitHub, &fork, &parent),
            ForkRelationship::SameNetworkIndirect
        );
        assert!(!ForkRelationship::SameNetworkIndirect.blocks_submission());
    }

    #[test]
    fn classify_fork_relationship_no_relationship_blocks() {
        let fork = network("bob/other", None, "bob/other");
        let parent = network("acme/widget", None, "acme/widget");
        assert_eq!(
            classify_fork_relationship(ForgeKind::GitHub, ForgeKind::GitHub, &fork, &parent),
            ForkRelationship::NoRelationship
        );
        assert!(ForkRelationship::NoRelationship.blocks_submission());
    }

    #[test]
    fn classify_fork_relationship_not_visible_is_not_treated_as_no_relationship() {
        let parent = network("acme/widget", None, "acme/widget");
        let outcome = classify_fork_relationship(
            ForgeKind::GitHub,
            ForgeKind::GitHub,
            &NetworkLookup::NotVisible,
            &parent,
        );
        assert_eq!(outcome, ForkRelationship::NotVisible);
        assert!(!outcome.blocks_submission());
    }

    #[test]
    fn classify_fork_relationship_cross_instance_has_no_path() {
        let fork = network("alice/widget", Some("acme/widget"), "acme/widget");
        let parent = network("acme/widget", None, "acme/widget");
        let outcome =
            classify_fork_relationship(ForgeKind::GitHub, ForgeKind::GitLab, &fork, &parent);
        assert_eq!(outcome, ForkRelationship::CrossInstance);
        assert!(!outcome.blocks_submission());
    }

    #[test]
    fn create_pull_request_routed_sends_gitlab_target_project_id_only_when_given() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let mut req = server.recv().unwrap();
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(json["target_project_id"], serde_json::json!(42));
            req.respond(
                tiny_http::Response::from_string(r#"{"iid": 1, "web_url": "http://x"}"#)
                    .with_status_code(201),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "alice%2Fwidget".to_string(),
            Some("tok".to_string()),
        );
        client
            .create_pull_request_routed("t", "b", "alias", "main", Some(42))
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn create_pull_request_without_target_project_id_omits_the_field() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let mut req = server.recv().unwrap();
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(json.get("target_project_id").is_none());
            req.respond(
                tiny_http::Response::from_string(r#"{"iid": 1, "web_url": "http://x"}"#)
                    .with_status_code(201),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "alice%2Fwidget".to_string(),
            Some("tok".to_string()),
        );
        client
            .create_pull_request("t", "b", "alias", "main")
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn find_open_pull_request_queries_githubs_documented_head_filter() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            let (path, query) = req.url().split_once('?').unwrap();
            assert_eq!(path, "/repos/acme/widget/pulls");
            assert!(
                query.contains("head=acme%3Aalias") || query.contains("head=acme:alias"),
                "{query}"
            );
            assert!(query.contains("state=open"), "{query}");
            req.respond(
                tiny_http::Response::from_string(
                    r#"[{"number":7,"html_url":"http://x/7","base":{"ref":"main"},"title":"T","body":"D"}]"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let found = client
            .find_open_pull_request("acme:alias")
            .unwrap()
            .expect("must find the open PR the mock server reports");
        assert_eq!(found.number, 7);
        assert_eq!(found.url, "http://x/7");
        assert_eq!(found.base, "main");
        assert_eq!(found.title, "T");
        assert_eq!(found.description, "D");
        handle.join().unwrap();
    }

    #[test]
    fn find_open_pull_request_is_none_when_githubs_list_is_empty() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(tiny_http::Response::from_string("[]").with_status_code(200))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert!(
            client
                .find_open_pull_request("acme:alias")
                .unwrap()
                .is_none()
        );
        handle.join().unwrap();
    }

    #[test]
    fn find_open_pull_request_queries_gitlabs_documented_source_branch_filter() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            let (path, query) = req.url().split_once('?').unwrap();
            assert_eq!(path, "/projects/alice%2Fwidget/merge_requests");
            assert!(query.contains("source_branch=alias"), "{query}");
            assert!(query.contains("state=opened"), "{query}");
            req.respond(
                tiny_http::Response::from_string(
                    r#"[{"iid":9,"web_url":"http://x/9","target_branch":"main","title":"T","description":"D"}]"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "alice%2Fwidget".to_string(),
            Some("tok".to_string()),
        );
        let found = client
            .find_open_pull_request("alias")
            .unwrap()
            .expect("must find the open MR the mock server reports");
        assert_eq!(found.number, 9);
        assert_eq!(found.base, "main");
        handle.join().unwrap();
    }

    #[test]
    fn same_repo_head_prefixes_github_with_the_repos_own_owner() {
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            "http://x".to_string(),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.same_repo_head("my-branch"), "acme:my-branch");
    }

    #[test]
    fn same_repo_head_leaves_gitlab_head_bare() {
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            "http://x".to_string(),
            "alice%2Fwidget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.same_repo_head("my-branch"), "my-branch");
    }

    #[test]
    fn find_open_pull_request_with_a_bare_github_head_would_match_any_open_pr() {
        // Regression guard for the bug this module's `same_repo_head` fixes:
        // GitHub's documented `head` filter silently returns every open PR,
        // unfiltered, when given a bare branch name instead of `owner:branch`
        // -- it does NOT error and does NOT scope to the named branch. Any
        // caller building a same-repo GitHub head must go through
        // `same_repo_head`, never pass a bare branch name directly, or a
        // "does a PR already exist for this branch" check silently adopts an
        // unrelated open PR. This test pins that raw (buggy-if-relied-on)
        // server behavior so a regression in `same_repo_head`'s callers is
        // caught even though this call site itself is intentionally bare.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            let (path, query) = req.url().split_once('?').unwrap();
            assert_eq!(path, "/repos/acme/widget/pulls");
            assert!(query.contains("head=totally-unrelated-branch"), "{query}");
            req.respond(
                tiny_http::Response::from_string(
                    r#"[{"number":2,"html_url":"http://x/2","base":{"ref":"staging"},"title":"unrelated","body":"unrelated"}]"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let found = client
            .find_open_pull_request("totally-unrelated-branch")
            .unwrap()
            .expect("bare head is ignored server-side and returns the unfiltered list");
        assert_eq!(found.number, 2);
        handle.join().unwrap();
    }

    #[test]
    fn forge_kind_detected_from_host() {
        assert_eq!(ForgeKind::from_host("github.com"), Some(ForgeKind::GitHub));
        assert_eq!(
            ForgeKind::from_host("gitlab.mycorp.internal"),
            Some(ForgeKind::GitLab)
        );
        assert_eq!(ForgeKind::from_host("bitbucket.org"), None);
    }

    #[test]
    fn default_api_bases() {
        assert_eq!(
            ForgeKind::GitHub.default_api_base("github.com"),
            "https://api.github.com"
        );
        assert_eq!(
            ForgeKind::GitHub.default_api_base("github.mycorp.com"),
            "https://github.mycorp.com/api/v3"
        );
        assert_eq!(
            ForgeKind::GitLab.default_api_base("gitlab.com"),
            "https://gitlab.com/api/v4"
        );
        assert_eq!(
            ForgeKind::GitLab.default_api_base("gitlab.mycorp.com"),
            "https://gitlab.mycorp.com/api/v4"
        );
    }

    #[test]
    fn gitlab_repo_path_is_percent_encoded() {
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            "https://gitlab.com/api/v4".to_string(),
            "group%2Fsub%2Fproj".to_string(),
            None,
        );
        assert_eq!(client.repo_label(), "group%2Fsub%2Fproj");
    }

    #[test]
    fn require_token_error_names_the_env_var() {
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            "https://api.github.com".to_string(),
            "acme/widget".to_string(),
            None,
        );
        let err = client.require_token().unwrap_err();
        assert!(err.contains("RALPHUS_GITHUB_TOKEN"), "{err}");
    }

    #[test]
    fn update_pull_request_base_sends_github_patch() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Patch);
            assert_eq!(req.url(), "/repos/acme/widget/pulls/7");
            req.respond(tiny_http::Response::from_string("{}").with_status_code(200))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        client.update_pull_request_base(7, "main").unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn update_pull_request_base_sends_gitlab_put() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Put);
            assert_eq!(req.url(), "/projects/group%2Fproj/merge_requests/9");
            req.respond(tiny_http::Response::from_string("{}").with_status_code(200))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        client.update_pull_request_base(9, "main").unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn update_pull_request_base_requires_token() {
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            "https://api.github.com".to_string(),
            "acme/widget".to_string(),
            None,
        );
        let err = client.update_pull_request_base(1, "main").unwrap_err();
        assert!(err.contains("RALPHUS_GITHUB_TOKEN"), "{err}");
    }

    #[test]
    fn extract_glab_token_handles_a_plain_token_line() {
        let text = "gitlab.com\n  Logged in to gitlab.com as colin\n  Token: glpat-abc123\n";
        assert_eq!(extract_glab_token(text), Some("glpat-abc123".to_string()));
    }

    #[test]
    fn extract_glab_token_handles_an_os_keyring_token_line() {
        // Real `glab auth status --show-token` output on Windows/macOS,
        // where the token is stored in the OS keyring rather than a plain
        // config file -- the bug this regression-tests: the old parser only
        // matched a literal "Token:" prefix and never fired on this wording.
        let text = "gitlab.com\n  \u{2713} Logged in to gitlab.com as colin (keyring)\n  \u{2713} Token found in operating system keyring: glpat-REDACTED-EXAMPLE-TOKEN0000000000\n";
        assert_eq!(
            extract_glab_token(text),
            Some("glpat-REDACTED-EXAMPLE-TOKEN0000000000".to_string())
        );
    }

    #[test]
    fn extract_glab_token_is_none_without_a_token_line() {
        let text =
            "gitlab.com\n  Not logged in to gitlab.com. Use `glab auth login` to authenticate.\n";
        assert_eq!(extract_glab_token(text), None);
    }

    #[test]
    fn get_pull_request_base_reads_github_base_ref() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            assert_eq!(req.url(), "/repos/acme/widget/pulls/5");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"base": {"ref": "feature-a"}, "updated_at": "2026-08-29T12:34:56.789Z"}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let state = client.get_pull_request_base_state(5).unwrap();
        assert_eq!(state.base, "feature-a");
        assert_eq!(
            state.updated_at_ms,
            chrono::DateTime::parse_from_rfc3339("2026-08-29T12:34:56.789Z")
                .unwrap()
                .timestamp_millis()
        );
        handle.join().unwrap();
    }

    #[test]
    fn get_pull_request_base_reads_gitlab_target_branch() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            assert_eq!(req.url(), "/projects/group%2Fproj/merge_requests/6");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"target_branch": "feature-a", "updated_at": "2026-08-29T12:34:56.789Z"}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.get_pull_request_base(6).unwrap(), "feature-a");
        handle.join().unwrap();
    }

    #[test]
    fn get_pull_request_base_errors_when_the_forge_response_omits_it() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(tiny_http::Response::from_string("{}").with_status_code(200))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let err = client.get_pull_request_base(7).unwrap_err();
        assert!(err.contains("base.ref"), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn get_pull_request_base_requires_token() {
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            "https://api.github.com".to_string(),
            "acme/widget".to_string(),
            None,
        );
        let err = client.get_pull_request_base(1).unwrap_err();
        assert!(err.contains("RALPHUS_GITHUB_TOKEN"), "{err}");
    }

    #[test]
    fn get_pull_request_state_reports_closed() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            assert_eq!(req.url(), "/repos/acme/widget/pulls/3");
            req.respond(
                tiny_http::Response::from_string(r#"{"state": "closed", "merged": false}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.get_pull_request_state(3).unwrap(), "closed");
        handle.join().unwrap();
    }

    #[test]
    fn get_pull_request_state_reports_merged_over_closed() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(r#"{"state": "closed", "merged": true}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.get_pull_request_state(3).unwrap(), "merged");
        handle.join().unwrap();
    }

    #[test]
    fn get_pull_request_state_normalizes_gitlab_opened() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/group%2Fproj/merge_requests/9");
            req.respond(
                tiny_http::Response::from_string(r#"{"state": "opened"}"#).with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.get_pull_request_state(9).unwrap(), "open");
        handle.join().unwrap();
    }

    #[test]
    fn create_stack_sends_ordered_pull_request_numbers() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let mut req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(req.url(), "/repos/acme/widget/stacks");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(payload["pull_requests"], serde_json::json!([3, 6, 7]));
            req.respond(
                tiny_http::Response::from_string(r#"{"number": 42}"#).with_status_code(201),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let stack = client.create_stack(&[3, 6, 7]).unwrap().unwrap();
        assert_eq!(stack.number, 42);
        handle.join().unwrap();
    }

    #[test]
    fn create_stack_is_a_no_op_for_gitlab() {
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            "https://gitlab.com/api/v4".to_string(),
            "group%2Fproj".to_string(),
            None,
        );
        assert_eq!(client.create_stack(&[1, 2]).unwrap(), None);
    }

    #[test]
    fn add_to_stack_sends_new_pull_request_numbers() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let mut req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(req.url(), "/repos/acme/widget/stacks/42/add");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(payload["pull_requests"], serde_json::json!([9]));
            req.respond(tiny_http::Response::from_string("{}").with_status_code(200))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        client.add_to_stack(42, &[9]).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn add_to_stack_is_a_no_op_for_gitlab() {
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            "https://gitlab.com/api/v4".to_string(),
            "group%2Fproj".to_string(),
            None,
        );
        assert!(client.add_to_stack(1, &[2]).is_ok());
    }

    #[test]
    fn unstack_posts_to_the_stacks_unstack_endpoint() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Post);
            assert_eq!(req.url(), "/repos/acme/widget/stacks/42/unstack");
            req.respond(tiny_http::Response::empty(204)).unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        client.unstack(42).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn stack_exists_treats_not_found_as_absent() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            assert_eq!(req.url(), "/repos/acme/widget/stacks/42");
            req.respond(tiny_http::Response::from_string("not found").with_status_code(404))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert!(!client.stack_exists(42).unwrap());
        handle.join().unwrap();
    }

    #[test]
    fn stack_exists_accepts_a_live_stack_response() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/stacks/42");
            req.respond(tiny_http::Response::from_string(r#"{"number":42}"#))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert!(client.stack_exists(42).unwrap());
        handle.join().unwrap();
    }

    #[test]
    fn a_401_evicts_the_cached_cli_fallback_token_but_a_404_does_not() {
        let host = "ral-401-evict-test.example.com";
        let key = (ForgeKind::GitHub, host.to_string());
        let seed_cache = |token: &str| {
            CLI_TOKEN_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_insert_with(HashMap::new)
                .insert(key.clone(), (token.to_string(), Instant::now()));
        };
        let cached_token = || {
            CLI_TOKEN_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(|m| m.get(&key))
                .map(|(t, _)| t.clone())
        };

        // A 404 (e.g. `stack_exists`'s not-found case above) must NOT evict --
        // it's a normal negative result, not evidence the token itself is bad.
        seed_cache("still-good");
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(tiny_http::Response::from_string("not found").with_status_code(404))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("still-good".to_string()),
        )
        .with_cli_token_host(host);
        assert!(!client.stack_exists(42).unwrap());
        handle.join().unwrap();
        assert_eq!(
            cached_token().as_deref(),
            Some("still-good"),
            "a 404 is a normal negative result, not proof the token is bad"
        );

        // A 401 IS evidence the token is bad -- evict so the next resolve
        // re-checks the CLI live instead of serving this token for the rest
        // of CLI_TOKEN_TTL.
        seed_cache("now-revoked");
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(tiny_http::Response::from_string("bad credentials").with_status_code(401))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("now-revoked".to_string()),
        )
        .with_cli_token_host(host);
        let _ = client.get_pull_request_state(1);
        handle.join().unwrap();
        assert_eq!(
            cached_token(),
            None,
            "a 401 must evict the cached CLI-fallback token"
        );
    }

    #[test]
    fn get_stack_pull_requests_returns_members_in_forge_order() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.method(), &tiny_http::Method::Get);
            assert_eq!(req.url(), "/repos/acme/widget/stacks/42");
            req.respond(tiny_http::Response::from_string(
                r#"{"number":42,"pull_requests":[{"number":3},{"number":6}]}"#,
            ))
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(
            client.get_stack_pull_requests(42).unwrap(),
            Some(vec![3, 6])
        );
        handle.join().unwrap();
    }

    #[test]
    fn get_stack_pull_requests_treats_not_found_as_absent() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/stacks/42");
            req.respond(tiny_http::Response::from_string("not found").with_status_code(404))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.get_stack_pull_requests(42).unwrap(), None);
        handle.join().unwrap();
    }

    #[test]
    fn unstack_is_a_no_op_for_gitlab() {
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            "https://gitlab.com/api/v4".to_string(),
            "group%2Fproj".to_string(),
            None,
        );
        assert!(client.unstack(1).is_ok());
    }

    /// GitLab has no native stacked-MR grouping, so moving a merge request's
    /// target branch needs none of the dissolve/re-register dance GitHub's
    /// stack restriction forces (see `crate::pr::repoint_stacked_prs`). The MR
    /// keeps its iid because the base move is a plain field update.
    ///
    /// Asserted as traffic rather than return values: the stack calls must not
    /// merely succeed, they must never reach the forge at all.
    #[test]
    fn gitlab_moves_a_base_without_ever_calling_a_stack_endpoint() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        let caller = std::thread::spawn(move || {
            let base = client.update_pull_request_base(7, "new-target");
            // Everything the GitHub recovery path would issue.
            let unstacked = client.unstack(42);
            let created = client.create_stack(&[7, 8]);
            let added = client.add_to_stack(42, &[9]);
            (base, unstacked, created, added)
        });

        let mut req = server
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .expect("the base move should reach the forge");
        assert_eq!(req.method(), &tiny_http::Method::Put);
        assert_eq!(req.url(), "/projects/group%2Fproj/merge_requests/7");
        let mut body = String::new();
        req.as_reader().read_to_string(&mut body).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(payload["target_branch"], "new-target");
        req.respond(tiny_http::Response::from_string("{}").with_status_code(200))
            .unwrap();

        let (base, unstacked, created, added) = caller.join().unwrap();
        base.unwrap();
        unstacked.unwrap();
        assert_eq!(created.unwrap(), None, "no stack is registered on GitLab");
        added.unwrap();

        assert!(
            server
                .recv_timeout(std::time::Duration::from_millis(500))
                .unwrap()
                .is_none(),
            "no stack endpoint may be called on GitLab"
        );
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ralphus-forge-test-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn g(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn remote_from_base_branch_detects_a_real_configured_remote() {
        let root = tmp_dir("remote-detect");
        g(&root, &["init", "--initial-branch", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );
        g(
            &root,
            &["remote", "add", "gitlab", "https://gitlab.com/a/b.git"],
        );

        assert_eq!(
            remote_from_base_branch(&root, "gitlab/main"),
            Some("gitlab".to_string())
        );
        assert_eq!(
            remote_from_base_branch(&root, "origin/main"),
            Some("origin".to_string())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_from_base_branch_is_none_for_a_plain_branch_or_unknown_remote() {
        let root = tmp_dir("remote-detect-none");
        g(&root, &["init", "--initial-branch", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );

        assert_eq!(remote_from_base_branch(&root, "main"), None);
        assert_eq!(
            remote_from_base_branch(&root, "colin/feature"),
            None,
            "a personal branch name that happens to contain a slash must not be mistaken for a remote"
        );
        assert_eq!(
            remote_from_base_branch(&root, "features/foo/bar"),
            None,
            "a slash-namespaced branch name (e.g. features/foo/bar) whose leading segment isn't a \
             configured remote must not be mistaken for one either"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_remote_name_falls_back_to_default_for_a_slash_namespaced_branch_name() {
        let root = tmp_dir("effective-remote-namespaced");
        g(&root, &["init", "--initial-branch", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );

        let cfg = ForgeConfig::default();
        assert_eq!(
            resolve_remote_name(&root, "features/foo/bar", &cfg),
            "origin",
            "the leading segment ('features') isn't a real remote, and 'features/foo/bar' has no \
             @{{u}} of its own either, so this must fall back to the config/origin default"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_remote_name_prefers_base_branchs_own_remote_prefix_over_the_config_default() {
        let root = tmp_dir("effective-remote");
        g(&root, &["init", "--initial-branch", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );
        g(
            &root,
            &["remote", "add", "gitlab", "https://gitlab.com/a/b.git"],
        );

        let cfg = ForgeConfig::default();
        assert_eq!(
            resolve_remote_name(&root, "gitlab/main", &cfg),
            "gitlab",
            "the base branch's own remote prefix must win over the project config default"
        );
        assert_eq!(
            resolve_remote_name(&root, "main", &cfg),
            "origin",
            "falls back to the config/origin default when base_branch names no remote and has no \
             @{{u}} of its own"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_remote_name_excluding_skips_a_registered_fork_remote() {
        let root = tmp_dir("effective-remote-exclude-fork");
        g(&root, &["init", "--initial-branch", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );
        g(
            &root,
            &["remote", "add", "fork", "https://example.com/me/b.git"],
        );

        let cfg = ForgeConfig::default();
        // Without exclusion, the base branch's own remote prefix (the
        // registered fork) would win, same as `resolve_remote_name`.
        assert_eq!(
            resolve_remote_name_excluding(&root, "fork/main", &cfg, None),
            "fork"
        );
        // With the fork excluded, it falls through to the config/origin
        // default instead of ever resolving to the excluded remote.
        assert_eq!(
            resolve_remote_name_excluding(&root, "fork/main", &cfg, Some("fork")),
            "origin"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_remote_name_honors_a_bare_local_branchs_own_at_u_upstream() {
        let root = tmp_dir("effective-remote-at-u");
        g(&root, &["init", "--initial-branch", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );
        g(
            &root,
            &["remote", "add", "alt", "https://alt.example.com/a/b.git"],
        );
        g(&root, &["commit", "--allow-empty", "--message", "init"]);
        g(&root, &["branch", "foo_branch_name"]);
        g(&root, &["config", "branch.foo_branch_name.remote", "alt"]);
        g(
            &root,
            &[
                "config",
                "branch.foo_branch_name.merge",
                "refs/heads/foo_branch_name",
            ],
        );
        // `@{u}` only resolves once the remote-tracking ref it points at
        // actually exists (as a real prior `git push -u alt foo_branch_name`
        // would have created) -- config alone isn't enough.
        let sha = g(&root, &["rev-parse", "foo_branch_name"]);
        g(
            &root,
            &["update-ref", "refs/remotes/alt/foo_branch_name", sha.trim()],
        );

        let cfg = ForgeConfig::default();
        assert_eq!(
            resolve_remote_name(&root, "foo_branch_name", &cfg),
            "alt",
            "a bare local branch name with no remote prefix must still resolve via its own @{{u}} \
             upstream rather than falling straight through to the config/origin default"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Set up a repo with `origin` on GitHub and a second remote on GitLab, so
    /// a wrong remote choice shows up as the wrong *forge host*, not just a
    /// different name -- the failure RAL-282 is actually about (a PR silently
    /// opened against the wrong forge instance).
    fn two_forge_repo(tag: &str, alt_remote: &str) -> std::path::PathBuf {
        let root = tmp_dir(tag);
        g(&root, &["init", "--initial-branch", "main"]);
        g(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/gh-owner/gh-repo.git",
            ],
        );
        g(
            &root,
            &[
                "remote",
                "add",
                alt_remote,
                "https://gitlab.com/gl-owner/gl-repo.git",
            ],
        );
        root
    }

    #[test]
    fn resolve_remote_resolves_the_forge_host_from_a_remote_prefixed_base_branch() {
        let root = two_forge_repo("forge-host-prefix", "alternativeremote");

        let client = resolve_remote(&root, "alternativeremote/foo", &ForgeConfig::default())
            .expect("resolve");
        assert_eq!(
            client.kind,
            ForgeKind::GitLab,
            "base_branch \"alternativeremote/foo\" must pick alternativeremote's URL\n             (GitLab), not origin's (GitHub), even with no [forge].remote configured"
        );
        assert_eq!(client.repo_path, "gl-owner%2Fgl-repo");
        assert_eq!(client.api_base, "https://gitlab.com/api/v4");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_remote_resolves_the_forge_host_from_a_bare_branchs_at_u_upstream() {
        let root = two_forge_repo("forge-host-at-u", "alt");
        g(&root, &["commit", "--allow-empty", "--message", "init"]);
        g(&root, &["branch", "foo_branch_name"]);
        g(&root, &["config", "branch.foo_branch_name.remote", "alt"]);
        g(
            &root,
            &[
                "config",
                "branch.foo_branch_name.merge",
                "refs/heads/foo_branch_name",
            ],
        );
        let sha = g(&root, &["rev-parse", "foo_branch_name"]);
        g(
            &root,
            &["update-ref", "refs/remotes/alt/foo_branch_name", sha.trim()],
        );

        let client =
            resolve_remote(&root, "foo_branch_name", &ForgeConfig::default()).expect("resolve");
        assert_eq!(
            client.kind,
            ForgeKind::GitLab,
            "a bare local base_branch tracking alt via @{{u}} must resolve the forge \n             against alt (GitLab), not fall through to origin (GitHub)"
        );
        assert_eq!(client.repo_path, "gl-owner%2Fgl-repo");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_remote_still_honors_forge_cfg_remote_when_the_base_branch_resolves_to_nothing() {
        let root = two_forge_repo("forge-host-cfg-fallback", "alt");

        let cfg = ForgeConfig {
            remote: Some("alt".to_string()),
            ..ForgeConfig::default()
        };
        let client = resolve_remote(&root, "main", &cfg).expect("resolve");
        assert_eq!(
            client.kind,
            ForgeKind::GitLab,
            "with no remote prefix and no @{{u}} upstream on \"main\", [forge].remote still decides"
        );

        let client = resolve_remote(&root, "main", &ForgeConfig::default()).expect("resolve");
        assert_eq!(
            client.kind,
            ForgeKind::GitHub,
            "and with nothing configured at all it lands on origin"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_from_branch_upstream_never_reads_the_current_checkouts_upstream() {
        let root = two_forge_repo("at-u-empty-guard", "alt");
        g(&root, &["commit", "--allow-empty", "--message", "init"]);
        // Give the *checked-out* branch an upstream on `alt`. An empty
        // base_branch must not pick that up: `format!("{base_branch}@{{u}}")`
        // would otherwise degrade to a bare `@{u}`.
        g(&root, &["config", "branch.main.remote", "alt"]);
        g(&root, &["config", "branch.main.merge", "refs/heads/main"]);
        let sha = g(&root, &["rev-parse", "main"]);
        g(&root, &["update-ref", "refs/remotes/alt/main", sha.trim()]);
        assert_eq!(
            remote_from_branch_upstream(&root, "main"),
            Some("alt".to_string()),
            "sanity: main really does track alt, so a bare @{{u}} here would resolve"
        );

        assert_eq!(
            remote_from_branch_upstream(&root, ""),
            None,
            "an empty base_branch must resolve to nothing rather than silently adopting\n             whatever the shared guardian repo happens to have checked out"
        );
        assert_eq!(
            resolve_remote_name(&root, "", &ForgeConfig::default()),
            "origin",
            "and it therefore falls through to the config/origin default"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_remote_name_falls_back_to_forge_cfg_remote_when_base_branch_has_no_remote_and_no_at_u()
     {
        let root = tmp_dir("effective-remote-cfg-fallback");
        g(&root, &["init", "--initial-branch", "main"]);
        g(
            &root,
            &["remote", "add", "origin", "https://example.com/a/b.git"],
        );
        g(
            &root,
            &[
                "remote",
                "add",
                "configured",
                "https://configured.example.com/a/b.git",
            ],
        );

        let cfg = ForgeConfig {
            remote: Some("configured".to_string()),
            ..ForgeConfig::default()
        };
        assert_eq!(
            resolve_remote_name(&root, "main", &cfg),
            "configured",
            "with no remote prefix and no @{{u}} upstream, [forge].remote is still the fallback \
             before origin"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn check_pr_ci_status_reports_github_conflict_before_any_check_call() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls/4");
            req.respond(
                tiny_http::Response::from_string(r#"{"mergeable_state": "dirty"}"#)
                    .with_status_code(200),
            )
            .unwrap();
            // A conflict verdict is terminal -- no check-runs call should follow.
            assert!(
                server
                    .recv_timeout(std::time::Duration::from_millis(200))
                    .unwrap()
                    .is_none()
            );
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let state = client.check_pr_ci_status(4).unwrap();
        assert_eq!(
            state,
            PrCiState::Failing(PrFailure {
                reason: "merge conflicts with the base branch".to_string(),
                job_url: None,
                log_text: None,
                checks: vec![],
            })
        );
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_a_failing_github_check_run() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls/4");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"mergeable_state": "clean", "head": {"sha": "deadbeef"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/commits/deadbeef/check-runs");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"check_runs": [{"name": "build", "status": "completed", "conclusion": "failure", "details_url": "https://ci.example/job/1", "output": {"text": "error: build failed\nsee above"}}]}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let state = client.check_pr_ci_status(4).unwrap();
        assert_eq!(
            state,
            PrCiState::Failing(PrFailure {
                reason: "check 'build' failed".to_string(),
                job_url: Some("https://ci.example/job/1".to_string()),
                log_text: Some("error: build failed\nsee above".to_string()),
                checks: vec![FailedCheck {
                    name: "build".to_string(),
                    job_url: Some("https://ci.example/job/1".to_string()),
                    log_text: Some("error: build failed\nsee above".to_string()),
                    failing_step: None,
                }],
            })
        );
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_every_failing_github_check_run_not_just_the_first() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/pulls/4");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"mergeable_state": "clean", "head": {"sha": "deadbeef"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/commits/deadbeef/check-runs");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"check_runs": [
                        {"name": "build", "status": "completed", "conclusion": "failure", "details_url": "https://ci.example/job/1", "output": {"text": "build broke"}},
                        {"name": "lint", "status": "completed", "conclusion": "success"},
                        {"name": "test", "status": "completed", "conclusion": "timed_out", "details_url": "https://ci.example/job/3", "output": {"summary": "timed out after 10m"}}
                    ]}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let state = client.check_pr_ci_status(4).unwrap();
        assert_eq!(
            state,
            PrCiState::Failing(PrFailure {
                reason: "2 checks failed: 'build', 'test'".to_string(),
                job_url: Some("https://ci.example/job/1".to_string()),
                log_text: Some("build broke".to_string()),
                checks: vec![
                    FailedCheck {
                        name: "build".to_string(),
                        job_url: Some("https://ci.example/job/1".to_string()),
                        log_text: Some("build broke".to_string()),
                        failing_step: None,
                    },
                    FailedCheck {
                        name: "test".to_string(),
                        job_url: Some("https://ci.example/job/3".to_string()),
                        log_text: Some("timed out after 10m".to_string()),
                        failing_step: None,
                    },
                ],
            }),
            "the passing 'lint' run must be excluded, and both failing runs -- not just the first -- \
             must be captured"
        );
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_github_pending_while_a_check_is_still_running() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"mergeable_state": "unknown", "head": {"sha": "deadbeef"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"check_runs": [{"name": "build", "status": "in_progress", "conclusion": null}]}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.check_pr_ci_status(4).unwrap(), PrCiState::Pending);
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_github_pending_after_a_rebase_moves_the_head_sha() {
        // RAL-462 (GitHub side of the same scenario the GitLab staleness
        // tests cover): a prior poll saw "oldsha" failing; the PR then gets
        // rebased/force-pushed to "newsha". Because every GitHub call here is
        // freshly re-fetched and its check-runs/status URLs are scoped to
        // whatever the *current* poll's head sha is, the stale failing
        // check-run for "oldsha" must never be consulted for "newsha" -- the
        // poll must report the new head's own (in this case still pending)
        // state instead of resurfacing the old failure.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"mergeable_state": "unstable", "head": {"sha": "oldsha"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/commits/oldsha/check-runs");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"check_runs": [{"name": "build", "status": "completed", "conclusion": "failure"}]}"#,
                )
                .with_status_code(200),
            )
            .unwrap();

            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"mergeable_state": "unknown", "head": {"sha": "newsha"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/commits/newsha/check-runs");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"check_runs": [{"name": "build", "status": "in_progress", "conclusion": null}]}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        assert!(matches!(
            client.check_pr_ci_status(4).unwrap(),
            PrCiState::Failing(_)
        ));
        assert_eq!(
            client.check_pr_ci_status(4).unwrap(),
            PrCiState::Pending,
            "polling again after the head sha moved must reflect the new commit's own state, \
             not the previous commit's failure"
        );
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_github_passing_when_everything_is_green() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"mergeable_state": "clean", "head": {"sha": "deadbeef"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"check_runs": [{"name": "build", "status": "completed", "conclusion": "success"}]}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/commits/deadbeef/status");
            req.respond(
                tiny_http::Response::from_string(r#"{"state": "success"}"#).with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.check_pr_ci_status(4).unwrap(), PrCiState::Passing);
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_github_passing_when_there_are_zero_legacy_statuses() {
        // Regression test for a real false-permanently-pending bug: a repo
        // whose CI reports exclusively through the Checks API (no legacy
        // commit statuses ever set) gets `{"state": "pending", "total_count":
        // 0}` from the combined-status endpoint forever, even after every
        // check-run has completed successfully. That default must not be
        // mistaken for a real in-flight legacy status.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"mergeable_state": "clean", "head": {"sha": "deadbeef"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"check_runs": [{"name": "build", "status": "completed", "conclusion": "success"}]}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/commits/deadbeef/status");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"state": "pending", "total_count": 0, "statuses": []}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.check_pr_ci_status(4).unwrap(), PrCiState::Passing);
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_github_pending_when_a_legacy_status_is_actually_pending() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"mergeable_state": "clean", "head": {"sha": "deadbeef"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(r#"{"check_runs": []}"#).with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/repos/acme/widget/commits/deadbeef/status");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"state": "pending", "total_count": 1, "statuses": [{"state": "pending", "context": "legacy-ci"}]}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.check_pr_ci_status(4).unwrap(), PrCiState::Pending);
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_gitlab_conflict_before_any_pipeline_call() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/group%2Fproj/merge_requests/9");
            req.respond(
                tiny_http::Response::from_string(r#"{"merge_status": "cannot_be_merged"}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        let state = client.check_pr_ci_status(9).unwrap();
        assert_eq!(
            state,
            PrCiState::Failing(PrFailure {
                reason: "merge conflicts with the target branch".to_string(),
                job_url: None,
                log_text: None,
                checks: vec![],
            })
        );
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_gitlab_pending_when_the_pipeline_is_stale_for_the_current_head() {
        // RAL-462: the MR moved (rebase/force-push/new commit) to "newsha",
        // but the `pipeline` field GitLab hands back still belongs to the
        // *previous* head ("oldsha") and reports it as failed -- a fresh
        // pipeline for "newsha" simply hasn't been created yet. The stale
        // pipeline's own verdict must not resurface as this PR's status.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/group%2Fproj/merge_requests/9");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"merge_status": "can_be_merged", "sha": "newsha", "pipeline": {"id": 55, "sha": "oldsha", "status": "failed", "web_url": "https://gitlab.example/pipelines/55"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        let state = client.check_pr_ci_status(9).unwrap();
        assert_eq!(
            state,
            PrCiState::Pending,
            "a pipeline belonging to a commit that's no longer the MR's head must not be reported \
             as the current status -- badge must read pending, not a stale failure/success"
        );
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_the_gitlab_pipelines_verdict_when_it_matches_the_current_head() {
        // Same shape as the staleness test above, but the pipeline's sha now
        // matches the MR's current head -- its "failed" verdict is genuine
        // and must still surface (the sha check must not swallow real
        // failures for the commit it's actually meant to report on).
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/group%2Fproj/merge_requests/9");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"merge_status": "can_be_merged", "sha": "samesha", "pipeline": {"id": 55, "sha": "samesha", "status": "success", "web_url": "https://gitlab.example/pipelines/55"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        let state = client.check_pr_ci_status(9).unwrap();
        assert_eq!(state, PrCiState::Passing);
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_a_failing_gitlab_pipeline_job_with_its_trace() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/group%2Fproj/merge_requests/9");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"merge_status": "can_be_merged", "pipeline": {"id": 55, "status": "failed", "web_url": "https://gitlab.example/pipelines/55"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(
                req.url(),
                "/projects/group%2Fproj/pipelines/55/jobs?scope[]=failed"
            );
            req.respond(
                tiny_http::Response::from_string(
                    r#"[{"id": 77, "name": "test", "web_url": "https://gitlab.example/jobs/77"}]"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/group%2Fproj/jobs/77/trace");
            req.respond(
                tiny_http::Response::from_string("FAIL: assertion failed\n").with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        let state = client.check_pr_ci_status(9).unwrap();
        assert_eq!(
            state,
            PrCiState::Failing(PrFailure {
                reason: "job 'test' failed".to_string(),
                job_url: Some("https://gitlab.example/jobs/77".to_string()),
                log_text: Some("FAIL: assertion failed\n".to_string()),
                checks: vec![FailedCheck {
                    name: "test".to_string(),
                    job_url: Some("https://gitlab.example/jobs/77".to_string()),
                    log_text: Some("FAIL: assertion failed\n".to_string()),
                    failing_step: None,
                }],
            })
        );
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_every_failing_gitlab_pipeline_job_not_just_the_first() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/group%2Fproj/merge_requests/9");
            req.respond(
                tiny_http::Response::from_string(
                    r#"{"merge_status": "can_be_merged", "pipeline": {"id": 55, "status": "failed", "web_url": "https://gitlab.example/pipelines/55"}}"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(
                req.url(),
                "/projects/group%2Fproj/pipelines/55/jobs?scope[]=failed"
            );
            req.respond(
                tiny_http::Response::from_string(
                    r#"[
                        {"id": 77, "name": "test", "web_url": "https://gitlab.example/jobs/77"},
                        {"id": 78, "name": "lint", "web_url": "https://gitlab.example/jobs/78"}
                    ]"#,
                )
                .with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/group%2Fproj/jobs/77/trace");
            req.respond(
                tiny_http::Response::from_string("FAIL: assertion failed\n").with_status_code(200),
            )
            .unwrap();
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/projects/group%2Fproj/jobs/78/trace");
            req.respond(tiny_http::Response::from_string("lint error\n").with_status_code(200))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        let state = client.check_pr_ci_status(9).unwrap();
        assert_eq!(
            state,
            PrCiState::Failing(PrFailure {
                reason: "2 jobs failed: 'test', 'lint'".to_string(),
                job_url: Some("https://gitlab.example/jobs/77".to_string()),
                log_text: Some("FAIL: assertion failed\n".to_string()),
                checks: vec![
                    FailedCheck {
                        name: "test".to_string(),
                        job_url: Some("https://gitlab.example/jobs/77".to_string()),
                        log_text: Some("FAIL: assertion failed\n".to_string()),
                        failing_step: None,
                    },
                    FailedCheck {
                        name: "lint".to_string(),
                        job_url: Some("https://gitlab.example/jobs/78".to_string()),
                        log_text: Some("lint error\n".to_string()),
                        failing_step: None,
                    },
                ],
            }),
            "both failed jobs -- not just the first -- must be captured, each with its own trace"
        );
        handle.join().unwrap();
    }

    #[test]
    fn check_pr_ci_status_reports_gitlab_pending_with_no_pipeline_yet() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string(r#"{"merge_status": "unchecked"}"#)
                    .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        assert_eq!(client.check_pr_ci_status(9).unwrap(), PrCiState::Pending);
        handle.join().unwrap();
    }

    // -----------------------------------------------------------------
    // RAL-366: structured forge errors + conditional (ETag) comment fetch
    // -----------------------------------------------------------------

    fn req_header(req: &tiny_http::Request, name: &'static str) -> Option<String> {
        req.headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    }

    #[test]
    fn parse_retry_after_parses_plain_delay_seconds() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after(" 5 "), Some(Duration::from_secs(5)));
    }

    #[test]
    fn parse_retry_after_is_none_for_an_http_date_or_garbage() {
        // Valid per HTTP spec, but neither forge actually sends this form --
        // deliberately not parsed (see `parse_retry_after`'s doc comment).
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn forge_error_is_rate_limited_only_for_429_and_403() {
        let make = |status: Option<u16>| ForgeError {
            status,
            retry_after: None,
            message: "x".to_string(),
        };
        assert!(make(Some(429)).is_rate_limited());
        assert!(make(Some(403)).is_rate_limited());
        assert!(!make(Some(404)).is_rate_limited());
        assert!(!make(Some(500)).is_rate_limited());
        assert!(!make(None).is_rate_limited());
    }

    #[test]
    fn forge_error_converts_to_string_preserving_the_message() {
        let e = ForgeError {
            status: Some(429),
            retry_after: Some(Duration::from_secs(30)),
            message: "forge API 429: slow down".to_string(),
        };
        let s: String = e.into();
        assert_eq!(s, "forge API 429: slow down");
    }

    #[test]
    fn describe_error_captures_status_and_integer_retry_after() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            req.respond(
                tiny_http::Response::from_string("rate limited")
                    .with_status_code(429)
                    .with_header(
                        tiny_http::Header::from_bytes(&b"Retry-After"[..], &b"30"[..]).unwrap(),
                    ),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let err = client
            .list_pr_comments_conditional(1, PrCommentEndpoint::Conversation, None)
            .unwrap_err();
        assert_eq!(err.status, Some(429));
        assert_eq!(err.retry_after, Some(Duration::from_secs(30)));
        assert!(err.is_rate_limited());
        handle.join().unwrap();
    }

    #[test]
    fn get_conditional_sends_if_none_match_and_reports_not_modified_on_304() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req_header(&req, "If-None-Match").as_deref(), Some("\"v1\""));
            req.respond(tiny_http::Response::from_string("").with_status_code(304))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let req = ureq::get(&format!("http://{addr}/x"));
        let result = client.get_conditional(req, Some("\"v1\"")).unwrap();
        assert!(matches!(result, ConditionalGet::NotModified));
        handle.join().unwrap();
    }

    #[test]
    fn list_pr_comments_conditional_github_fetches_both_endpoints_with_fresh_etags() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let conv = server.recv().unwrap();
            assert_eq!(
                conv.url(),
                "/repos/acme/widget/issues/7/comments?per_page=100"
            );
            assert_eq!(req_header(&conv, "If-None-Match"), None);
            conv.respond(
                tiny_http::Response::from_string(
                    r#"[{"id":1,"user":{"login":"alice"},"body":"hi","created_at":"2024-01-01T00:00:00Z"}]"#,
                )
                .with_status_code(200)
                .with_header(tiny_http::Header::from_bytes(&b"ETag"[..], &b"\"c1\""[..]).unwrap()),
            )
            .unwrap();

            let review = server.recv().unwrap();
            assert_eq!(
                review.url(),
                "/repos/acme/widget/pulls/7/comments?per_page=100"
            );
            review
                .respond(
                    tiny_http::Response::from_string(
                        r#"[{"id":2,"user":{"login":"bob"},"body":"inline","created_at":"2024-01-02T00:00:00Z"}]"#,
                    )
                    .with_status_code(200)
                    .with_header(
                        tiny_http::Header::from_bytes(&b"ETag"[..], &b"\"r1\""[..]).unwrap(),
                    ),
                )
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let conv = client
            .list_pr_comments_conditional(7, PrCommentEndpoint::Conversation, None)
            .unwrap();
        let CommentsPoll::Modified { comments, etag } = conv else {
            panic!("expected a fresh body");
        };
        assert_eq!(
            comments,
            vec![PrComment {
                external_id: "1".to_string(),
                author: "alice".to_string(),
                body: "hi".to_string(),
                created_at: "2024-01-01T00:00:00Z".to_string(),
            }]
        );
        assert_eq!(etag.as_deref(), Some("\"c1\""));

        let review = client
            .list_pr_comments_conditional(7, PrCommentEndpoint::Review, None)
            .unwrap();
        let CommentsPoll::Modified { comments, etag } = review else {
            panic!("expected a fresh body");
        };
        assert_eq!(comments[0].author, "bob");
        assert_eq!(etag.as_deref(), Some("\"r1\""));
        handle.join().unwrap();
    }

    #[test]
    fn list_pr_comments_conditional_reports_not_modified_and_costs_no_reparse() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req_header(&req, "If-None-Match").as_deref(), Some("\"c1\""));
            req.respond(tiny_http::Response::from_string("").with_status_code(304))
                .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitHub,
            format!("http://{addr}"),
            "acme/widget".to_string(),
            Some("tok".to_string()),
        );
        let result = client
            .list_pr_comments_conditional(7, PrCommentEndpoint::Conversation, Some("\"c1\""))
            .unwrap();
        assert!(matches!(result, CommentsPoll::NotModified));
        handle.join().unwrap();
    }

    #[test]
    fn list_pr_comments_conditional_gitlab_uses_the_single_notes_endpoint_for_either_source() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(
                req.url(),
                "/projects/group%2Fproj/merge_requests/3/notes?per_page=100"
            );
            req.respond(
                tiny_http::Response::from_string(
                    r#"[{"id":5,"system":false,"author":{"username":"carol"},"body":"note","created_at":"2024-01-03T00:00:00Z"}]"#,
                )
                .with_status_code(200),
            )
            .unwrap();
        });
        let client = ForgeClient::new(
            ForgeKind::GitLab,
            format!("http://{addr}"),
            "group%2Fproj".to_string(),
            Some("tok".to_string()),
        );
        // `Review` is meaningless for GitLab -- must route to the exact same
        // single notes endpoint as `Conversation`, not error or 404.
        let result = client
            .list_pr_comments_conditional(3, PrCommentEndpoint::Review, None)
            .unwrap();
        let CommentsPoll::Modified { comments, .. } = result else {
            panic!("expected a fresh body");
        };
        assert_eq!(comments[0].author, "carol");
        handle.join().unwrap();
    }
}
