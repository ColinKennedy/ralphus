//! Machine strict mode: a **development tool** that catches work for a
//! remote machine running on the daemon's own host instead.
//!
//! A squad, review or PR assigned to a remote machine should never spawn a
//! local `git`, shell command or agent: everything it does belongs on that
//! machine. When some code path forgets, the work still "succeeds" -- against
//! the daemon's checkout, on the daemon's host -- and nothing says so. This
//! module makes that visible.
//!
//! The thread doing remote-assigned work enters a [`RemoteScope`] naming the
//! machine. The local execution choke points ([`crate::vcs::GitVcs`], the
//! local command-proof runner, the local runner behind
//! [`crate::remote_runner::MachineRouter`]) call [`check_local`] before
//! spawning anything. Inside a scope that call:
//!
//! - **strict mode on** -- refuses: the operation fails with a message naming
//!   the call site and the machine, so the leak cannot pass unnoticed;
//! - **strict mode off** -- allows it and logs a warning once per call site
//!   and machine, so leaks stay visible without breaking anything.
//!
//! Outside a scope (all local work) it does nothing at all.
//!
//! ## Not for production servers
//!
//! Strict mode turns every local fallback ralphus has not removed yet into a
//! failed step. That is what developing remote support needs -- each failure
//! is a bug report -- and exactly what a production server does not want, where
//! a degraded-but-working step beats a stopped one. It is off by default.
//!
//! Switched by `[machine] strict = true` in the global `config.toml`, with
//! `RALPHUS_MACHINE_STRICT=1|0` overriding it. Read once, at first use; a
//! change needs a daemon restart.
//!
//! A scope is per thread: work a scoped thread hands to another thread is not
//! covered. That can only miss a leak, never fail purely local work.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::Deserialize;

/// The env var that overrides the `[machine] strict` config key.
pub const STRICT_ENV: &str = "RALPHUS_MACHINE_STRICT";

const UNKNOWN: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);

thread_local! {
    static SCOPE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

static WARNED: std::sync::LazyLock<parking_lot::Mutex<HashSet<(String, String)>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashSet::new()));

#[derive(Debug, Default, Deserialize)]
struct StrictFile {
    #[serde(default)]
    machine: Option<StrictTable>,
}

#[derive(Debug, Default, Deserialize)]
struct StrictTable {
    #[serde(default)]
    strict: Option<bool>,
}

/// Parse an env-var value: `1`/`true`/`on`/`yes` or `0`/`false`/`off`/`no`,
/// case-insensitively. `None` for anything else, which is then ignored.
fn parse_switch(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// `[machine] strict` from the global config file. A missing file or key is
/// `false`; an unreadable or mistyped one is `false` with a warning.
fn strict_from_config() -> bool {
    let Some(path) = crate::config::global_config_path().filter(|p| p.is_file()) else {
        return false;
    };
    let parsed = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|text| toml::from_str::<StrictFile>(&text).map_err(|e| e.to_string()));
    match parsed {
        Ok(file) => file.machine.and_then(|m| m.strict).unwrap_or(false),
        Err(e) => {
            // ralphus[ignore-rlog-pair]: read lazily from deep inside git/runner calls, where no store is in reach
            crate::rlog!(
                WARNING,
                "ralphus [machine] could not read [machine] strict from {}: {e}; strict mode stays off",
                path.display()
            );
            false
        }
    }
}

/// Whether strict mode is on: [`STRICT_ENV`] when set to a recognised value,
/// else `[machine] strict` in the global config.
#[must_use]
pub fn enabled() -> bool {
    match STATE.load(Ordering::Relaxed) {
        ON => true,
        OFF => false,
        _ => {
            let on = std::env::var(STRICT_ENV)
                .ok()
                .and_then(|v| parse_switch(&v))
                .unwrap_or_else(strict_from_config);
            STATE.store(if on { ON } else { OFF }, Ordering::Relaxed);
            on
        }
    }
}

/// Force strict mode on or off for this process (tests only).
#[cfg(test)]
pub fn set_enabled_for_test(on: bool) {
    STATE.store(if on { ON } else { OFF }, Ordering::Relaxed);
}

/// The startup warning, when strict mode is on: it is easy to leave enabled
/// on a server by accident, and the cost there is failed work.
pub fn announce(store: &crate::store::Store) {
    if enabled() {
        crate::cartographer::Note::new("daemon")
            .level(crate::logging::LogLevel::WARNING)
            .emit(
                store,
                "machine strict mode is enabled -- intended for ralphus development, not \
                 production: any local git, command or agent run for work assigned to a remote \
                 machine will fail",
                serde_json::json!({ "machine_strict": true }),
            );
    }
}

/// Marks the current thread as doing work for a remote machine until dropped.
#[must_use = "the scope ends when this guard is dropped"]
pub struct RemoteScope {
    _private: (),
}

impl Drop for RemoteScope {
    fn drop(&mut self) {
        SCOPE.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// Enter a scope for work assigned to `machine`. `None` -- no guard -- when
/// `machine` is unset, empty or `local`, so callers can pass whatever machine
/// their entity resolved to.
#[must_use = "the scope ends when the returned guard is dropped"]
pub fn enter(machine: Option<&str>) -> Option<RemoteScope> {
    let machine = machine.map(str::trim).filter(|m| {
        !m.is_empty() && !m.eq_ignore_ascii_case(ralphus_core::schema::LOCAL_MACHINE)
    })?;
    SCOPE.with(|s| s.borrow_mut().push(machine.to_string()));
    Some(RemoteScope { _private: () })
}

/// Enter a scope for guardian `id`'s review machine, for callers that hold
/// only the id. `None` when the review is local, gone, or unreadable -- a
/// missing scope only means strict mode does not check this work.
#[must_use = "the scope ends when the returned guard is dropped"]
pub fn enter_for_guardian(store: &crate::store_lock::StoreHandle, id: &str) -> Option<RemoteScope> {
    let machine = store
        .with_read_snapshot(|conn| crate::store::Store::guardian_machine_conn(conn, id))
        .ok()
        .flatten();
    enter(machine.as_deref())
}

/// The remote machine the current thread is working for, if any.
#[must_use]
pub fn current_machine() -> Option<String> {
    SCOPE.with(|s| s.borrow().last().cloned())
}

/// Called by a local execution choke point before it spawns anything.
///
/// `site` names the choke point (`"git"`, `"command"`, `"agent"`) and
/// `detail` what it was about to run. Outside a [`RemoteScope`] this is `Ok`.
///
/// # Errors
/// Inside a scope with strict mode on: a message naming the site, the
/// operation and the machine.
pub fn check_local(site: &str, detail: impl FnOnce() -> String) -> Result<(), String> {
    let Some(machine) = current_machine() else {
        return Ok(());
    };
    let detail = detail();
    if enabled() {
        return Err(format!(
            "machine strict mode: refused to run {site} on the daemon host for work assigned to \
             machine \"{machine}\" ({detail}). This is a ralphus bug -- the operation should run \
             on that machine. Strict mode is a development tool; turn it off with \
             {STRICT_ENV}=0 or `[machine] strict = false` to fall back to the daemon host."
        ));
    }
    if WARNED.lock().insert((site.to_string(), machine.clone())) {
        // ralphus[ignore-rlog-pair]: called from inside git/runner choke points, which may already hold the store lock
        crate::rlog!(
            WARNING,
            "ralphus [machine] ran {site} on the daemon host for work assigned to machine \
             \"{machine}\" ({detail}); further {site} fallbacks for this machine are not logged"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_work_is_never_checked() {
        set_enabled_for_test(true);
        assert!(enter(None).is_none());
        assert!(enter(Some("local")).is_none());
        assert!(enter(Some("  ")).is_none());
        assert!(check_local("git", || "status".to_string()).is_ok());
    }

    #[test]
    fn a_reviews_scope_follows_its_assigned_machine() {
        let store = std::sync::Arc::new(crate::store_lock::StoreMutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let (remote, local) = {
            let guard = store.lock();
            let remote = guard.create_guardian("r", "main", "/repo").unwrap();
            guard
                .set_guardian_machine(&remote, Some("fleet:box"))
                .unwrap();
            let local = guard.create_guardian("l", "main", "/repo").unwrap();
            (remote, local)
        };
        {
            let _scope = enter_for_guardian(&store, &remote);
            assert_eq!(current_machine().as_deref(), Some("fleet:box"));
        }
        assert!(enter_for_guardian(&store, &local).is_none());
        assert!(enter_for_guardian(&store, "guardian-missing").is_none());
        assert_eq!(current_machine(), None);
    }

    #[test]
    fn strict_mode_refuses_local_work_inside_a_remote_scope() {
        set_enabled_for_test(true);
        let scope = enter(Some("fleet:box"));
        let err = check_local("git", || "push origin review".to_string()).expect_err("must refuse");
        assert!(err.contains("fleet:box"), "{err}");
        assert!(err.contains("push origin review"), "{err}");
        assert!(err.contains(STRICT_ENV), "{err}");
        drop(scope);
        assert!(check_local("git", || "status".to_string()).is_ok());
    }

    #[test]
    fn without_strict_mode_local_work_is_allowed_inside_a_scope() {
        set_enabled_for_test(false);
        let _scope = enter(Some("fleet:box"));
        assert!(check_local("git", || "status".to_string()).is_ok());
    }

    #[test]
    fn scopes_nest_and_unwind() {
        let outer = enter(Some("fleet:a"));
        {
            let _inner = enter(Some("fleet:b"));
            assert_eq!(current_machine().as_deref(), Some("fleet:b"));
        }
        assert_eq!(current_machine().as_deref(), Some("fleet:a"));
        drop(outer);
        assert_eq!(current_machine(), None);
    }

    #[test]
    fn a_scope_does_not_leak_to_other_threads() {
        set_enabled_for_test(true);
        let _scope = enter(Some("fleet:box"));
        let other = std::thread::spawn(|| check_local("git", || "status".to_string()))
            .join()
            .unwrap();
        assert!(other.is_ok());
    }

    #[test]
    fn strict_mode_stops_a_real_local_git_call_inside_a_remote_scope() {
        set_enabled_for_test(true);
        let dir = std::env::temp_dir();
        assert!(crate::guardian_merge::git(&dir, &["--version"]).is_ok());
        let _scope = enter(Some("fleet:box"));
        let err = crate::guardian_merge::git(&dir, &["--version"]).expect_err("must refuse");
        assert!(err.contains("machine strict mode"), "{err}");
        assert!(err.contains("git --version"), "{err}");
    }

    #[test]
    fn strict_mode_stops_a_real_local_command_inside_a_remote_scope() {
        set_enabled_for_test(true);
        let _scope = enter(Some("fleet:box"));
        let (ok, out) = crate::proof::run_command_proof_capture(
            &std::env::temp_dir().to_string_lossy(),
            "echo hi",
            &opentelemetry::Context::new(),
            &std::collections::BTreeMap::new(),
            &crate::cancel::CancelToken::new(),
        );
        assert!(!ok);
        assert!(out.contains("machine strict mode"), "{out}");
    }

    #[test]
    fn switch_values_parse() {
        for on in ["1", "true", "ON", " yes "] {
            assert_eq!(parse_switch(on), Some(true), "{on}");
        }
        for off in ["0", "false", "Off", "no"] {
            assert_eq!(parse_switch(off), Some(false), "{off}");
        }
        assert_eq!(parse_switch("maybe"), None);
    }
}
