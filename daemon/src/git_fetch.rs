//! Fetch named refs without launching `git`. Callers keep their own ref locks.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use git2::{Cred, CredentialType, FetchOptions, ProxyOptions, RemoteCallbacks, Repository};

const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Returns `None` only when the command needs Git-specific behavior that
/// libgit2 cannot reproduce. A failed in-process fetch is not retried through
/// Git: that would duplicate network work and could hide an authentication
/// failure.
pub(crate) fn try_fetch(root: &Path, args: &[&str]) -> Option<Result<(), String>> {
    try_fetch_with_token(root, args, None)
}

/// A forge token is supplied only for a remote that belongs to the same
/// resolved forge client. It is used for HTTPS and never sent over SSH.
pub(crate) fn try_fetch_with_token(
    root: &Path,
    args: &[&str],
    token: Option<&str>,
) -> Option<Result<(), String>> {
    let ["fetch", "--no-write-fetch-head", remote_name, refspecs @ ..] = args else {
        return None;
    };
    if !ralphus_runner::git_inproc::enabled()
        || refspecs.is_empty()
        || std::env::var_os("GIT_CONFIG_COUNT").is_some()
        || std::env::var_os("GIT_CONFIG_PARAMETERS").is_some()
    {
        return None;
    }
    let repo = Repository::open(root).ok()?;
    let config = repo.config().ok()?;
    let mut remote = repo.find_remote(remote_name).ok()?;
    let url = remote.url()?.to_string();
    let token_host = ralphus_core::git_remote::parse_remote_url(&url).map(|(host, _)| host);
    let direct_token_scheme = if url.starts_with("https://") {
        "https://"
    } else if url.starts_with("http://") {
        "http://"
    } else {
        ""
    };
    if url.starts_with("http://")
        && !(url.starts_with("http://127.0.0.1:")
            || url.starts_with("http://localhost:")
            || url.starts_with("http://[::1]:"))
    {
        return None;
    }
    if is_ssh_url(&url)
        && (config.get_string("core.sshCommand").is_ok()
            || std::env::var_os("GIT_SSH_COMMAND").is_some()
            || std::env::var_os("GIT_SSH").is_some())
    {
        return None;
    }
    if !(url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("ssh://")
        || url.starts_with("file://")
        || is_ssh_url(&url)
        || Path::new(&url).exists()
        || root.join(&url).exists())
    {
        return None;
    }

    let started = Instant::now();
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(|url, username, allowed| {
        if allowed.contains(CredentialType::SSH_KEY) {
            return ssh_credential(username.unwrap_or("git"));
        }
        if allowed.contains(CredentialType::USERNAME) {
            return Cred::username(username.unwrap_or("git"));
        }
        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            let same_host = token_host.as_deref().is_some_and(|expected| {
                !direct_token_scheme.is_empty()
                    && url.starts_with(direct_token_scheme)
                    && ralphus_core::git_remote::parse_remote_url(url)
                        .is_some_and(|(actual, _)| actual.eq_ignore_ascii_case(expected))
            });
            if let Some(token) = token.filter(|_| same_host) {
                return Cred::userpass_plaintext(username.unwrap_or("x-access-token"), token);
            }
            return Cred::credential_helper(&config, url, username);
        }
        if allowed.contains(CredentialType::DEFAULT) {
            return Cred::default();
        }
        Err(git2::Error::from_str("no supported fetch credential"))
    });
    callbacks.transfer_progress(|_| started.elapsed() < FETCH_TIMEOUT);
    callbacks.sideband_progress(|_| started.elapsed() < FETCH_TIMEOUT);
    let mut options = FetchOptions::new();
    options
        .update_fetchhead(false)
        .remote_callbacks(callbacks)
        .proxy_options({
            let mut proxy = ProxyOptions::new();
            proxy.auto();
            proxy
        });
    match remote.fetch(refspecs, Some(&mut options), None) {
        Ok(()) => Some(Ok(())),
        // Git's SSH transport understands more key and host configuration
        // than libgit2. An auth failure is safe to retry through that path.
        Err(e) if is_ssh_url(&url) && e.code() == git2::ErrorCode::Auth => None,
        Err(e) => Some(Err(format!(
            "in-process git fetch from {remote_name} failed: {e}"
        ))),
    }
}

fn is_ssh_url(url: &str) -> bool {
    url.starts_with("ssh://") || (!url.contains("://") && url.contains('@'))
}

fn ssh_credential(username: &str) -> Result<Cred, git2::Error> {
    if std::env::var_os("SSH_AUTH_SOCK").is_some() {
        return Cred::ssh_key_from_agent(username);
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or_else(|| git2::Error::from_str("no home directory for SSH key"))?;
    for key in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let path = home.join(".ssh").join(key);
        if path.is_file() {
            return Cred::ssh_key(username, None, &path, None);
        }
    }
    Cred::ssh_key_from_agent(username)
}
