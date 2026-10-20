//! The `gh pr edit --attach` fallback for PR exhibits: the first `gh` release
//! that has it, and reading `gh --version`. Shared by the daemon (which uses
//! the fallback) and `ralphus check health` (which warns when it is missing).

/// The first `gh` with `--attach` (released 2026-09-01).
pub const MIN_VERSION: (u32, u32, u32) = (2, 99, 0);

/// What a GitHub project loses without a usable `gh` fallback, for health
/// warnings.
pub const IMPACT: &str = "Live-action exhibits (screenshots, GIFs, videos) upload to GitHub PRs \
through the GitHub API first and fall back to `gh pr edit --attach`. Without gh 2.99+ logged \
in, a PR whose direct upload fails gets no screenshots or GIFs; they stay on the ralphus board.";

/// The remediation for a missing, old or logged-out `gh`.
pub const REMEDIATION: &str = "Install or update the GitHub CLI to 2.99 or later \
(https://cli.github.com) and run `gh auth login`.";

/// Parse `gh --version`'s first line (`gh version 2.99.0 (2026-09-01)`).
#[must_use]
pub fn parse_version(output: &str) -> Option<(u32, u32, u32)> {
    let line = output.lines().next()?;
    let version = line.strip_prefix("gh version ")?.split_whitespace().next()?;
    let mut parts = version.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// Whether `version` has `--attach`.
#[must_use]
pub fn supports_attach(version: (u32, u32, u32)) -> bool {
    version >= MIN_VERSION
}

/// `major.minor.patch`.
#[must_use]
pub fn format_version(version: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(
            parse_version("gh version 2.93.0 (2026-05-27)\nhttps://github.com/cli/cli"),
            Some((2, 93, 0))
        );
        assert_eq!(parse_version("gh version 2.99.1"), Some((2, 99, 1)));
        assert_eq!(parse_version("gh version 3.0"), Some((3, 0, 0)));
        assert!(!supports_attach((2, 93, 0)));
        assert!(supports_attach((2, 99, 0)));
        assert!(supports_attach((3, 0, 0)));
        assert_eq!(parse_version("garbage"), None);
        assert_eq!(format_version((2, 99, 0)), "2.99.0");
    }
}
