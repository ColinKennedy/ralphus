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

use std::path::Path;

use crate::config::ForgeConfig;

/// Which forge a repository is hosted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Live base-ref metadata used to reconcile forge-authored base edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestBaseState {
    pub base: String,
    pub updated_at_ms: i64,
}

/// A created pull/merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPr {
    /// PR number (GitHub) / MR `iid` (GitLab) — repo-scoped, not a global id.
    pub number: i64,
    /// Web URL a human can open.
    pub url: String,
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
        }
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
                let resp = send(
                    ureq::post(&url)
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Accept", "application/vnd.github+json"),
                    &payload,
                )?;
                let number = resp["number"]
                    .as_i64()
                    .ok_or_else(|| format!("unexpected GitHub PR response shape: {resp}"))?;
                let url = resp["html_url"].as_str().unwrap_or_default().to_string();
                Ok(CreatedPr { number, url })
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
                let resp = send(ureq::post(&url).set("PRIVATE-TOKEN", token), &payload)?;
                let number = resp["iid"]
                    .as_i64()
                    .ok_or_else(|| format!("unexpected GitLab MR response shape: {resp}"))?;
                let url = resp["web_url"].as_str().unwrap_or_default().to_string();
                Ok(CreatedPr { number, url })
            }
        }
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
        let resp = get(ureq::get(&url).set("PRIVATE-TOKEN", token))?;
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
                let resp = get(ureq::get(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Accept", "application/vnd.github+json"))?;
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
                let resp = get(ureq::get(&url).set("PRIVATE-TOKEN", token))?;
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
                let resp = get(ureq::get(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Accept", "application/vnd.github+json"))?;
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
                let resp = get(ureq::get(&url).set("PRIVATE-TOKEN", token))?;
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
                send(
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
                send(ureq::put(&url).set("PRIVATE-TOKEN", token), &payload)?;
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
                send(
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
                send(ureq::put(&url).set("PRIVATE-TOKEN", token), &payload)?;
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
                send(
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
                send(ureq::post(&url).set("PRIVATE-TOKEN", token), &payload)?;
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
        let resp = send(
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
        send(
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
            Err(e) => Err(describe_error(e)),
        };
        match &result {
            Ok(exists) => crate::rlog!(
                DEBUG,
                "ralphus [forge] get stack done kind={} repo={} stack={stack_number} exists={exists}",
                self.kind.as_str(),
                self.repo_path
            ),
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
            Err(e) => Err(describe_error(e)),
        };
        match &result {
            Ok(Some(members)) => crate::rlog!(
                DEBUG,
                "ralphus [forge] get stack members done kind={} repo={} stack={stack_number} pull_requests={members:?}",
                self.kind.as_str(),
                self.repo_path
            ),
            Ok(None) => crate::rlog!(
                DEBUG,
                "ralphus [forge] get stack members done kind={} repo={} stack={stack_number} missing",
                self.kind.as_str(),
                self.repo_path
            ),
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
            .map_err(describe_error)?;
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
                // GitHub models a PR as an issue for general conversation comments.
                let url = format!(
                    "{}/repos/{}/issues/{number}/comments",
                    self.api_base, self.repo_path
                );
                let resp = get(ureq::get(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Accept", "application/vnd.github+json"))?;
                let items = resp
                    .as_array()
                    .ok_or_else(|| format!("unexpected GitHub comments response shape: {resp}"))?;
                Ok(items
                    .iter()
                    .map(|c| PrComment {
                        external_id: c["id"].as_i64().unwrap_or_default().to_string(),
                        author: c["user"]["login"].as_str().unwrap_or_default().to_string(),
                        body: c["body"].as_str().unwrap_or_default().to_string(),
                        created_at: c["created_at"].as_str().unwrap_or_default().to_string(),
                    })
                    .collect())
            }
            ForgeKind::GitLab => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{number}/notes",
                    self.api_base, self.repo_path
                );
                let resp = get(ureq::get(&url).set("PRIVATE-TOKEN", token))?;
                let items = resp
                    .as_array()
                    .ok_or_else(|| format!("unexpected GitLab notes response shape: {resp}"))?;
                Ok(items
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
                    .collect())
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
        let resp = get(req).ok()?;
        let b64 = resp["content"].as_str()?;
        decode_base64_maybe_wrapped(b64)
    }

    fn gitlab_default_branch(&self, token: Option<&str>) -> Option<String> {
        let url = format!("{}/projects/{}", self.api_base, self.repo_path);
        let mut req = ureq::get(&url);
        if let Some(t) = token {
            req = req.set("PRIVATE-TOKEN", t);
        }
        let resp = get(req).ok()?;
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

fn send(req: ureq::Request, payload: &serde_json::Value) -> Result<serde_json::Value, String> {
    let resp = req
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .map_err(describe_error)?;
    parse_body(resp)
}

fn get(req: ureq::Request) -> Result<serde_json::Value, String> {
    let resp = req.call().map_err(describe_error)?;
    parse_body(resp)
}

fn describe_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            format!("forge API {code}: {body}")
        }
        other => format!("forge API: {other}"),
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
/// this machine. Best-effort only — returns `None` on any failure (CLI not
/// installed, not logged in, unexpected output shape) and callers should
/// treat that exactly like "no token configured" rather than surfacing a
/// separate error. Only usable when the daemon process runs on the same
/// machine as that CLI login; a remote/CI daemon still needs the env var.
///
/// TODO: Replace with real user-service authentication once RAL-245 is complete.
fn resolve_cli_token(kind: ForgeKind, host: &str) -> Option<String> {
    match kind {
        ForgeKind::GitHub => {
            let mut cmd = std::process::Command::new("gh");
            cmd.arg("auth").arg("token");
            if !host.eq_ignore_ascii_case("github.com") {
                cmd.arg("--hostname").arg(host);
            }
            let output = cmd.output().ok()?;
            if !output.status.success() {
                return None;
            }
            let token = String::from_utf8(output.stdout).ok()?;
            let token = token.trim();
            if token.is_empty() {
                None
            } else {
                Some(token.to_string())
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
                .ok()?;
            let combined = [output.stdout, output.stderr].concat();
            let text = String::from_utf8(combined).ok()?;
            extract_glab_token(&text)
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
        let Ok(resp) = get(ureq::get(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/vnd.github+json"))
        else {
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
        get(ureq::get(&url).set("PRIVATE-TOKEN", token)).ok()
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
    // TODO: Replace with real user-service authentication once RAL-245 is complete.
    let token = std::env::var(&token_env)
        .ok()
        .or_else(|| resolve_cli_token(kind, &host));
    if token.is_some() && std::env::var(&token_env).is_err() {
        // ralphus[ignore-rlog-pair]: this provider boundary has no Store; its caller records the structured workflow outcome
        crate::rlog!(
            DEBUG,
            "ralphus [forge] resolved token via CLI fallback kind={} host={host}",
            kind.as_str()
        );
    }

    let repo_path = match kind {
        ForgeKind::GitHub => path,
        ForgeKind::GitLab => path.replace('/', "%2F"),
    };

    Ok(ForgeClient::new(kind, api_base, repo_path, token))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
