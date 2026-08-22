//! Global CLI options, extracted from anywhere in argv before the command
//! tree is parsed -- mirrors `__main__.py::_extract_global_json_flag` (every
//! literal `--json` token is pulled out wherever it appears, since argparse
//! would otherwise reject it after the subcommand name) and the
//! `--daemon-url`/`$RALPHUS_DAEMON_URL` convention threaded through every
//! subparser in Python. Both are genuinely global here (same value
//! everywhere), so extracting them once up front is a legitimate
//! simplification of the per-subparser declaration Python used.

use crate::client::DaemonClient;

#[derive(Debug, Clone)]
pub struct GlobalOpts {
    pub daemon_url: String,
    pub json: bool,
}

impl GlobalOpts {
    #[must_use]
    pub fn client(&self) -> DaemonClient {
        DaemonClient::new(self.daemon_url.clone())
    }
}

/// Removes every `--json` and `--daemon-url <url>` occurrence from `args`,
/// returning the resolved [`GlobalOpts`] and the remaining tokens. Stops
/// removing at a bare `--` (end-of-options marker), matching Python's own
/// stop-at-`--` behavior for its global-flag extraction pass.
#[must_use]
pub fn extract_global_opts(args: &[String]) -> (GlobalOpts, Vec<String>) {
    let mut json = false;
    let mut daemon_url: Option<String> = None;
    let mut remaining: Vec<String> = Vec::new();
    let mut i = 0;
    let mut stopped = false;
    while i < args.len() {
        let a = &args[i];
        if stopped {
            remaining.push(a.clone());
            i += 1;
            continue;
        }
        if a == "--" {
            stopped = true;
            remaining.push(a.clone());
            i += 1;
            continue;
        }
        if a == "--json" {
            json = true;
            i += 1;
            continue;
        }
        if a == "--daemon-url" {
            if let Some(v) = args.get(i + 1) {
                daemon_url = Some(v.clone());
                i += 2;
                continue;
            }
        }
        if let Some(v) = a.strip_prefix("--daemon-url=") {
            daemon_url = Some(v.to_string());
            i += 1;
            continue;
        }
        remaining.push(a.clone());
        i += 1;
    }

    let daemon_url = daemon_url
        .or_else(|| std::env::var("RALPHUS_DAEMON_URL").ok())
        .unwrap_or_else(|| DaemonClient::default_url().to_string());
    (GlobalOpts { daemon_url, json }, remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn extracts_json_flag_from_anywhere() {
        let (opts, rest) = extract_global_opts(&v(&["status", "--json", "run-1"]));
        assert!(opts.json);
        assert_eq!(rest, v(&["status", "run-1"]));
    }

    #[test]
    fn extracts_daemon_url_space_and_equals_forms() {
        let (opts, _) = extract_global_opts(&v(&["status", "--daemon-url", "http://x:1"]));
        assert_eq!(opts.daemon_url, "http://x:1");
        let (opts2, _) = extract_global_opts(&v(&["status", "--daemon-url=http://y:2"]));
        assert_eq!(opts2.daemon_url, "http://y:2");
    }

    #[test]
    fn defaults_when_absent() {
        let (opts, rest) = extract_global_opts(&v(&["status"]));
        assert!(!opts.json);
        assert_eq!(opts.daemon_url, DaemonClient::default_url());
        assert_eq!(rest, v(&["status"]));
    }

    #[test]
    fn stops_extracting_after_bare_separator() {
        let (opts, rest) = extract_global_opts(&v(&["quick-start", "--", "--json"]));
        assert!(!opts.json);
        assert_eq!(rest, v(&["quick-start", "--", "--json"]));
    }
}
