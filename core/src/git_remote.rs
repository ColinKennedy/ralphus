//! Git remote URL normalization and remote selection by URL.
//!
//! Shared by the daemon (PR/MR routing) and `ralphus initialize server`, so both
//! agree on when two remote URLs name the same repository.

/// Parse a git remote URL into `(host, path)`, where `path` has no leading
/// slash, trailing slash, or `.git` suffix and credentials are dropped.
/// Supports the three shapes git itself accepts: `git@host:owner/repo.git`,
/// `https://host/owner/repo.git`, and `ssh://git@host/owner/repo.git`.
#[must_use]
pub fn parse_remote_url(url: &str) -> Option<(String, String)> {
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

/// Whether two remote URLs name the same repository after normalization
/// (scheme, credentials, trailing `/` and `.git` are ignored; the host is
/// compared case-insensitively). Unparseable URLs only match when textually
/// identical.
#[must_use]
pub fn remote_urls_match(a: &str, b: &str) -> bool {
    match (parse_remote_url(a), parse_remote_url(b)) {
        (Some((host_a, path_a)), Some((host_b, path_b))) => {
            host_a.eq_ignore_ascii_case(&host_b) && path_a == path_b
        }
        _ => a.trim() == b.trim() && !a.trim().is_empty(),
    }
}

/// The name of the remote in `remotes` (`(name, url)` pairs) whose URL names
/// the same repository as `url`. With several matches the first listed wins,
/// the same convention the daemon's PR routing follows. Remotes named in
/// `exclude` are skipped.
#[must_use]
pub fn find_remote_for_url<'a>(
    remotes: &'a [(String, String)],
    url: &str,
    exclude: &[&str],
) -> Option<&'a str> {
    remotes
        .iter()
        .filter(|(name, _)| !exclude.contains(&name.as_str()))
        .find(|(_, remote_url)| remote_urls_match(remote_url, url))
        .map(|(name, _)| name.as_str())
}

/// The remote a project's upstream URL most plausibly lives on when the user
/// has not said: the first remote that is not `fork_url`, preferring `origin`.
#[must_use]
pub fn default_upstream_remote<'a>(
    remotes: &'a [(String, String)],
    fork_url: Option<&str>,
) -> Option<&'a (String, String)> {
    let candidates = || {
        remotes
            .iter()
            .filter(|(_, url)| fork_url.is_none_or(|fork| !remote_urls_match(url, fork)))
    };
    candidates()
        .find(|(name, _)| name == "origin")
        .or_else(|| candidates().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remotes(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, u)| ((*n).to_string(), (*u).to_string()))
            .collect()
    }

    #[test]
    fn urls_match_across_scheme_credentials_suffixes_and_host_case() {
        assert!(remote_urls_match(
            "git@github.com:acme/app.git",
            "https://user:tok@GitHub.com/acme/app/"
        ));
        assert!(remote_urls_match(
            "ssh://git@github.com/acme/app",
            "https://github.com/acme/app.git"
        ));
        assert!(!remote_urls_match(
            "git@github.com:acme/app.git",
            "git@github.com:me/app.git"
        ));
        assert!(!remote_urls_match("", ""));
    }

    #[test]
    fn find_matches_a_remote_not_named_origin() {
        let r = remotes(&[
            ("origin", "git@github.com:me/app.git"),
            ("upstream", "git@github.com:acme/app.git"),
        ]);
        assert_eq!(
            find_remote_for_url(&r, "https://github.com/acme/app", &[]),
            Some("upstream")
        );
    }

    #[test]
    fn find_returns_first_listed_match_and_none_without_match() {
        let r = remotes(&[
            ("b", "git@github.com:acme/app.git"),
            ("a", "git@github.com:acme/app.git"),
        ]);
        assert_eq!(
            find_remote_for_url(&r, "git@github.com:acme/app.git", &[]),
            Some("b")
        );
        assert_eq!(
            find_remote_for_url(&r, "git@github.com:other/app.git", &[]),
            None
        );
        assert_eq!(
            find_remote_for_url(&r, "git@github.com:acme/app.git", &["b", "a"]),
            None
        );
    }

    #[test]
    fn default_upstream_skips_the_fork_and_prefers_origin() {
        let r = remotes(&[
            ("origin", "git@github.com:me/app.git"),
            ("upstream", "git@github.com:acme/app.git"),
        ]);
        let fork = "https://github.com/me/app.git";
        assert_eq!(
            default_upstream_remote(&r, Some(fork)).map(|(n, _)| n.as_str()),
            Some("upstream")
        );
        assert_eq!(
            default_upstream_remote(&r, None).map(|(n, _)| n.as_str()),
            Some("origin")
        );
        let only_fork = remotes(&[("origin", "git@github.com:me/app.git")]);
        assert!(default_upstream_remote(&only_fork, Some(fork)).is_none());
    }
}
