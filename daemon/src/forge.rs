//! Forge integration (RAL-117): create and query pull/merge requests against
//! GitHub or GitLab over their REST APIs directly — no `gh` CLI or other
//! external tool dependency, matching this daemon's existing habit of calling
//! provider HTTP APIs straight with `ureq` (see `chat_client.rs`).
//!
//! Routing is config-driven: [`crate::config::ForgeConfig`] (the `[forge]`
//! TOML table, global or per-project) can pin the forge kind, remote name, API
//! base URL, and token env var explicitly. Any field left unset falls back to
//! autodetection from the repository's `git remote get-url` (see
//! [`resolve_remote`]).
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
    fn from_host(host: &str) -> Option<Self> {
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

/// A created pull/merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPr {
    /// PR number (GitHub) / MR `iid` (GitLab) — repo-scoped, not a global id.
    pub number: i64,
    /// Web URL a human can open.
    pub url: String,
}

/// A resolved connection to one forge repository: enough to create PRs, list
/// comments, and fetch a PR template. Built by [`resolve_remote`].
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
        crate::rlog!(
            INFO,
            "ralphus [forge] create pr start kind={} repo={} head={head} base={base}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.create_pull_request_inner(title, body, head, base);
        match &result {
            Ok(pr) => crate::rlog!(
                INFO,
                "ralphus [forge] create pr done kind={} repo={} number={} url={}",
                self.kind.as_str(),
                self.repo_path,
                pr.number,
                pr.url
            ),
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
                let payload = serde_json::json!({
                    "source_branch": head,
                    "target_branch": base,
                    "title": title,
                    "description": body,
                });
                let resp = send(ureq::post(&url).set("PRIVATE-TOKEN", token), &payload)?;
                let number = resp["iid"]
                    .as_i64()
                    .ok_or_else(|| format!("unexpected GitLab MR response shape: {resp}"))?;
                let url = resp["web_url"].as_str().unwrap_or_default().to_string();
                Ok(CreatedPr { number, url })
            }
        }
    }

    /// Retarget an already-open PR/MR's base/target branch (RAL-190: keeps a
    /// stacked PR's base in sync after its review is reordered). Best-effort
    /// from the caller's point of view -- callers should log-and-continue on
    /// error rather than aborting a whole reorder over one forge call, since
    /// the local `base_ref` record is updated regardless. Logs the outbound
    /// call (start/done/error) via `rlog!`.
    pub fn update_pull_request_base(&self, number: i64, new_base: &str) -> Result<(), String> {
        crate::rlog!(
            INFO,
            "ralphus [forge] update pr base start kind={} repo={} number={number} new_base={new_base}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.update_pull_request_base_inner(number, new_base);
        match &result {
            Ok(()) => crate::rlog!(
                INFO,
                "ralphus [forge] update pr base done kind={} repo={} number={number}",
                self.kind.as_str(),
                self.repo_path
            ),
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

    /// List human-authored comments on a PR/MR, oldest first. GitLab's
    /// system-generated notes (label changes, etc.) are filtered out since
    /// they are never actionable feedback. Logs the outbound call
    /// (start/done/error) via `rlog!`.
    pub fn list_pr_comments(&self, number: i64) -> Result<Vec<PrComment>, String> {
        crate::rlog!(
            DEBUG,
            "ralphus [forge] list pr comments start kind={} repo={} number={number}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.list_pr_comments_inner(number);
        match &result {
            Ok(comments) => crate::rlog!(
                DEBUG,
                "ralphus [forge] list pr comments done kind={} repo={} number={number} count={}",
                self.kind.as_str(),
                self.repo_path,
                comments.len()
            ),
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
        crate::rlog!(
            DEBUG,
            "ralphus [forge] fetch pr template start kind={} repo={}",
            self.kind.as_str(),
            self.repo_path
        );
        let result = self.fetch_pr_template_inner();
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
fn parse_remote_url(url: &str) -> Option<(String, String)> {
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
    } else if let Some(colon) = url.strip_prefix("git@").and_then(|r| {
        let idx = r.find(':')?;
        Some((r, idx))
    }) {
        let (rest, idx) = colon;
        (&rest[..idx], &rest[idx + 1..])
    } else {
        return None;
    };
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some((host.to_string(), path.to_string()))
}

/// Resolve the effective forge client for a git repository at `root`,
/// combining the layered [`ForgeConfig`] (explicit overrides) with
/// autodetection from `git remote get-url <remote>` (host → kind, path →
/// repo). Fails only when the remote can't be read/parsed or no forge kind
/// can be determined — a missing token is not an error here (see
/// [`ForgeClient::require_token`]). Logs the resolved kind/repo/api_base (or
/// the failure reason) via `rlog!`.
pub fn resolve_remote(root: &Path, cfg: &ForgeConfig) -> Result<ForgeClient, String> {
    let result = resolve_remote_inner(root, cfg);
    match &result {
        Ok(client) => crate::rlog!(
            DEBUG,
            "ralphus [forge] resolved kind={} repo={} api_base={}",
            client.kind.as_str(),
            client.repo_path,
            client.api_base
        ),
        Err(e) => crate::rlog!(WARNING, "ralphus [forge] resolve remote failed: {e}"),
    }
    result
}

fn resolve_remote_inner(root: &Path, cfg: &ForgeConfig) -> Result<ForgeClient, String> {
    let remote_name = cfg.remote.as_deref().unwrap_or("origin");
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
    let token = std::env::var(&token_env).ok();

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
}
