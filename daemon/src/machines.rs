//! Machine provider registry (RAL-185).
//!
//! A task/cell/proof/review may declare `machine = "<scheme>:<uri>"`. The
//! daemon never interprets `<uri>` — it looks `<scheme>` up in this registry to
//! find a registered **provider program**, and hands the uri to it verbatim.
//! `<scheme>` is a provider name like `incredibuild`; `<uri>` is whatever that
//! provider defines (`A`, a hostname, a URL, ...).
//!
//! This mirrors the project registry (`Store::register_project`) deliberately:
//! same "register once as an admin action, reference by name from TOML" shape,
//! same "core validates syntax offline, the daemon resolves against the store
//! at submit time" split.
//!
//! ## Why registration is not declarable in TOML
//!
//! A provider entry names a **program the daemon will run**. If a submitted
//! task file could both *name* and *define* a provider, then `ralphus submit`
//! would be equivalent to arbitrary code execution by anyone who can write a
//! TOML file. So a task file may only ever *reference* an already-registered
//! scheme; registering one is a separate, explicit administrative action
//! (`ralphus machine register`, or `POST /api/machines`). There is intentionally
//! no TOML syntax for it.
//!
//! ## The built-in scheme
//!
//! One scheme is always available and is not stored in the table:
//!
//! - `local` — the daemon's own host. Also the implicit default when `machine`
//!   is unset anywhere in the inheritance chain.

use serde::Serialize;

use crate::store::{Result as StoreResult, Store, now_ms};

/// The built-in scheme naming the daemon's own host.
pub const LOCAL_SCHEME: &str = ralphus_core::schema::LOCAL_MACHINE;

/// Schemes that resolve without a registry row.
pub const BUILTIN_SCHEMES: &[&str] = &[LOCAL_SCHEME];

/// Schemes that may never be registered as a machine provider, but which are
/// **not** resolvable as machines either.
///
/// `ralphus` carries two unrelated meanings already — the worktree placeholder
/// (`ralphus:new-worktree/<branch>`) and the RAL-188 entity URI
/// (`ralphus:/SQUAD[...]`). Letting someone register it as a *third* thing would
/// make `machine = "ralphus:..."` genuinely ambiguous to read, so registration
/// is refused up front rather than resolved into one of the three by accident.
pub const RESERVED_SCHEMES: &[&str] = &["ralphus"];

/// Whether `scheme` is a built-in that needs no registry entry.
#[must_use]
pub fn is_builtin_scheme(scheme: &str) -> bool {
    BUILTIN_SCHEMES
        .iter()
        .any(|b| b.eq_ignore_ascii_case(scheme))
}

/// Whether `scheme` may not be registered — either because it is a built-in
/// (registering would shadow it) or because it is reserved for another
/// meaning entirely (see [`RESERVED_SCHEMES`]).
#[must_use]
pub fn is_unregistrable_scheme(scheme: &str) -> bool {
    is_builtin_scheme(scheme)
        || RESERVED_SCHEMES
            .iter()
            .any(|b| b.eq_ignore_ascii_case(scheme))
}

/// A registered machine provider.
#[derive(Debug, Clone, Serialize)]
pub struct MachineProviderView {
    /// Unique scheme name, e.g. `"incredibuild"`. Matched case-insensitively
    /// against the `<scheme>` half of a `machine` value.
    pub scheme: String,
    /// Human-readable description, shown in listings and error messages.
    pub description: String,
    /// Absolute path to the provider program the daemon invokes.
    pub program: String,
    /// Arguments always prepended to the provider invocation, before the verb
    /// and the uri. Lets one program back several schemes (e.g. a single
    /// script dispatched with `--flavor incredibuild`).
    pub args: Vec<String>,
    /// Provider-contract version this entry was registered against. The daemon
    /// refuses to invoke a provider whose declared version it does not
    /// support, rather than guessing at a mismatched protocol.
    pub protocol_version: i64,
    /// Registration time (Unix epoch milliseconds).
    pub created_at_ms: i64,
    /// When this provider was last probed for reachability, or `None` if never.
    ///
    /// `None` is meaningfully different from either outcome: the board shows
    /// "not checked" rather than implying a machine is healthy or broken on no
    /// evidence at all.
    pub last_check_ms: Option<i64>,
    /// Whether that probe succeeded. `None` when never probed.
    pub last_check_ok: Option<bool>,
    /// The provider's own note on success, or the failure reason.
    pub last_check_note: Option<String>,
    /// Whether this provider implements the `channel` verb — one long-lived
    /// process serving many commands, instead of a spawn per command
    /// (RAL-185 D7).
    ///
    /// Opt-in at registration rather than probed: a provider that quietly does
    /// not support it would otherwise cost a failed spawn on every first
    /// command, and the fallback is silent enough that nobody would notice the
    /// waste.
    pub supports_channel: bool,
}

/// The provider-contract version this daemon implements. A provider
/// registered against a different version is rejected at resolution time with
/// an explicit error rather than invoked and hoped for.
pub const PROTOCOL_VERSION: i64 = 1;

/// How a `machine` value resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedMachine {
    /// Runs on the daemon's own host — the pre-RAL-185 behavior.
    Local,
    /// Runs via a provider. `uri` is opaque and passed through verbatim.
    Provider {
        /// The matched scheme.
        scheme: String,
        /// The opaque provider-defined remainder.
        uri: String,
    },
}

impl ResolvedMachine {
    /// Whether this resolves to the daemon's own host.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }
}

/// Why a `machine` value could not be resolved against the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The value did not parse as `local` or `scheme:uri`.
    Malformed(String),
    /// The scheme parsed but names no registered provider.
    UnknownScheme {
        /// The unregistered scheme.
        scheme: String,
        /// Every scheme that *is* available, for the error message.
        known: Vec<String>,
    },
    /// The provider is registered but declares an unsupported contract version.
    UnsupportedProtocol {
        /// The scheme whose provider mismatched.
        scheme: String,
        /// The version the provider was registered against.
        found: i64,
        /// The version this daemon implements.
        expected: i64,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(m) => write!(
                f,
                "invalid machine {m:?}: expected \"local\" or \"<provider>:<uri>\""
            ),
            Self::UnknownScheme { scheme, known } => {
                if known.is_empty() {
                    write!(
                        f,
                        "machine provider {scheme:?} is not registered, and no providers are registered yet — register one with `ralphus machine register`"
                    )
                } else {
                    write!(
                        f,
                        "machine provider {scheme:?} is not registered (available: {})",
                        known.join(", ")
                    )
                }
            }
            Self::UnsupportedProtocol {
                scheme,
                found,
                expected,
            } => write!(
                f,
                "machine provider {scheme:?} declares contract version {found}, but this daemon implements {expected}"
            ),
        }
    }
}

impl Store {
    /// Register (or update) a machine provider. Upserts on `scheme`, matching
    /// [`Store::register_project`]'s behavior.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn register_machine_provider(
        &self,
        scheme: &str,
        description: &str,
        program: &str,
        args: &[String],
        protocol_version: i64,
        supports_channel: bool,
    ) -> StoreResult<()> {
        let args_json = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "INSERT INTO machine_providers(scheme, description, program, args, protocol_version, created_at_ms, supports_channel)
             VALUES(?,?,?,?,?,?,?)
             ON CONFLICT(scheme) DO UPDATE SET
                description=excluded.description,
                program=excluded.program,
                args=excluded.args,
                protocol_version=excluded.protocol_version,
                supports_channel=excluded.supports_channel",
            rusqlite::params![
                scheme.trim().to_lowercase(),
                description,
                program,
                args_json,
                protocol_version,
                now_ms(),
                i64::from(supports_channel)
            ],
        )?;
        crate::rlog!(
            INFO,
            "ralphus [store] machine provider {scheme:?} registered program={program} protocol={protocol_version}"
        );
        let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
            level: crate::logging::LogLevel::INFO,
            source: "store",
            message: "machine provider registered",
            scope: Some("machine"),
            squad_id: None,
            guardian_id: None,
            cell_id: None,
            task: None,
            log_path: None,
            payload: serde_json::json!({
                "scheme": scheme,
                "program": program,
                "protocol_version": protocol_version,
            }),
        });
        Ok(())
    }

    /// Record the outcome of a reachability probe against `scheme`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn record_machine_check(
        &self,
        scheme: &str,
        ok: bool,
        note: Option<&str>,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE machine_providers SET last_check_ms=?, last_check_ok=?, last_check_note=?
             WHERE scheme = ?",
            rusqlite::params![now_ms(), i64::from(ok), note, scheme.trim().to_lowercase()],
        )?;
        Ok(())
    }

    /// Every registered provider, newest first.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn list_machine_providers(&self) -> StoreResult<Vec<MachineProviderView>> {
        let mut stmt = self.conn.prepare(
            "SELECT scheme, description, program, args, protocol_version, created_at_ms,
                    last_check_ms, last_check_ok, last_check_note, supports_channel
             FROM machine_providers ORDER BY created_at_ms DESC, scheme",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let args: String = r.get(3)?;
                Ok(MachineProviderView {
                    scheme: r.get(0)?,
                    description: r.get(1)?,
                    program: r.get(2)?,
                    args: serde_json::from_str(&args).unwrap_or_default(),
                    protocol_version: r.get(4)?,
                    created_at_ms: r.get(5)?,
                    last_check_ms: r.get(6)?,
                    last_check_ok: r.get::<_, Option<i64>>(7)?.map(|v| v != 0),
                    last_check_note: r.get(8)?,
                    supports_channel: r.get::<_, i64>(9)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One provider by scheme (case-insensitive), or `None`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn get_machine_provider(&self, scheme: &str) -> StoreResult<Option<MachineProviderView>> {
        Ok(self
            .list_machine_providers()?
            .into_iter()
            .find(|p| p.scheme.eq_ignore_ascii_case(scheme.trim())))
    }

    /// Remove a provider. Returns whether a row was deleted.
    ///
    /// Deliberately does **not** check whether any stored squad still references
    /// the scheme: those squads already resolved their machines when they were
    /// submitted, and a historical squad's record should not block cleaning up
    /// the registry. A *new* submission naming a deregistered scheme fails at
    /// submit with [`ResolveError::UnknownScheme`].
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn deregister_machine_provider(&self, scheme: &str) -> StoreResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM machine_providers WHERE scheme = ?",
            rusqlite::params![scheme.trim().to_lowercase()],
        )?;
        if n > 0 {
            crate::rlog!(INFO, "ralphus [store] machine provider {scheme:?} removed");
            let _ = self.cartographer_log(crate::cartographer::CartographerEntry {
                level: crate::logging::LogLevel::INFO,
                source: "store",
                message: "machine provider removed",
                scope: Some("machine"),
                squad_id: None,
                guardian_id: None,
                cell_id: None,
                task: None,
                log_path: None,
                payload: serde_json::json!({ "scheme": scheme }),
            });
        }
        Ok(n > 0)
    }

    /// Resolve a `machine` value against the registry.
    ///
    /// `None` (the field unset anywhere in the inheritance chain) resolves to
    /// [`ResolvedMachine::Local`], so a task file that never mentions `machine`
    /// behaves exactly as it did before RAL-185.
    ///
    /// # Errors
    /// Returns [`ResolveError`] when the value is malformed, names an
    /// unregistered scheme, or names a provider with an unsupported contract
    /// version.
    pub fn resolve_machine(
        &self,
        machine: Option<&str>,
    ) -> std::result::Result<ResolvedMachine, ResolveError> {
        let Some(raw) = machine.map(str::trim).filter(|m| !m.is_empty()) else {
            return Ok(ResolvedMachine::Local);
        };
        let parsed = ralphus_core::schema::parse_machine(raw)
            .map_err(|_| ResolveError::Malformed(raw.to_string()))?;
        let (scheme, uri) = match parsed {
            ralphus_core::schema::MachineRef::Local => return Ok(ResolvedMachine::Local),
            ralphus_core::schema::MachineRef::Provider { scheme, uri } => (scheme, uri),
        };
        if scheme.eq_ignore_ascii_case(LOCAL_SCHEME) {
            return Ok(ResolvedMachine::Local);
        }
        if is_builtin_scheme(scheme) {
            return Ok(ResolvedMachine::Provider {
                scheme: scheme.to_lowercase(),
                uri: uri.to_string(),
            });
        }
        let found = self.get_machine_provider(scheme).map_err(|_| {
            // A store failure here is indistinguishable to the caller from
            // "not registered", and the actionable advice is the same.
            ResolveError::UnknownScheme {
                scheme: scheme.to_string(),
                known: vec![],
            }
        })?;
        let Some(provider) = found else {
            let mut known: Vec<String> = self
                .list_machine_providers()
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.scheme)
                .collect();
            known.extend(BUILTIN_SCHEMES.iter().map(|s| (*s).to_string()));
            known.sort();
            return Err(ResolveError::UnknownScheme {
                scheme: scheme.to_string(),
                known,
            });
        };
        if provider.protocol_version != PROTOCOL_VERSION {
            return Err(ResolveError::UnsupportedProtocol {
                scheme: scheme.to_string(),
                found: provider.protocol_version,
                expected: PROTOCOL_VERSION,
            });
        }
        Ok(ResolvedMachine::Provider {
            scheme: provider.scheme,
            uri: uri.to_string(),
        })
    }
}

/// A persisted async `exec` handle for one in-flight remote cell
/// (RAL-201), reconciled against on daemon startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteExecHandle {
    pub squad_id: String,
    pub cell_id: String,
    pub scheme: String,
    pub uri: String,
    pub handle: String,
}

impl Store {
    /// Record the opaque handle a provider returned for an async `exec`, so a
    /// daemon restart can reconcile it (see [`Self::all_remote_exec_handles`])
    /// instead of silently orphaning whatever is still running remotely.
    /// Upserts on `(squad_id, cell_id)` — a cell only ever has one
    /// in-flight handle at a time.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn save_remote_exec_handle(
        &self,
        squad_id: &str,
        cell_id: &str,
        scheme: &str,
        uri: &str,
        handle: &str,
    ) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO remote_exec_handles(squad_id, cell_id, scheme, uri, handle, created_at_ms)
             VALUES(?,?,?,?,?,?)
             ON CONFLICT(squad_id, cell_id) DO UPDATE SET
                scheme=excluded.scheme, uri=excluded.uri, handle=excluded.handle,
                created_at_ms=excluded.created_at_ms",
            rusqlite::params![squad_id, cell_id, scheme, uri, handle, now_ms()],
        )?;
        Ok(())
    }

    /// Clear a cell's persisted handle once its `exec` has finished (by any
    /// outcome — done, failed, cancelled, timed out, cost-exceeded) or a
    /// restart has already reconciled it. Not finding one is not an error —
    /// every caller here is cleaning up and may race a concurrent clear.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn clear_remote_exec_handle(&self, squad_id: &str, cell_id: &str) -> StoreResult<()> {
        self.conn.execute(
            "DELETE FROM remote_exec_handles WHERE squad_id = ? AND cell_id = ?",
            rusqlite::params![squad_id, cell_id],
        )?;
        Ok(())
    }

    /// Every persisted remote `exec` handle, for startup reconciliation.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn all_remote_exec_handles(&self) -> StoreResult<Vec<RemoteExecHandle>> {
        let mut stmt = self
            .conn
            .prepare("SELECT squad_id, cell_id, scheme, uri, handle FROM remote_exec_handles")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RemoteExecHandle {
                    squad_id: r.get(0)?,
                    cell_id: r.get(1)?,
                    scheme: r.get(2)?,
                    uri: r.get(3)?,
                    handle: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    #[test]
    fn unset_machine_resolves_local() {
        let s = store();
        assert_eq!(s.resolve_machine(None).unwrap(), ResolvedMachine::Local);
        assert_eq!(s.resolve_machine(Some("")).unwrap(), ResolvedMachine::Local);
        assert_eq!(
            s.resolve_machine(Some("   ")).unwrap(),
            ResolvedMachine::Local
        );
    }

    #[test]
    fn local_literal_and_local_scheme_both_resolve_local() {
        let s = store();
        assert_eq!(
            s.resolve_machine(Some("local")).unwrap(),
            ResolvedMachine::Local
        );
        assert_eq!(
            s.resolve_machine(Some("LOCAL")).unwrap(),
            ResolvedMachine::Local
        );
    }

    #[test]
    fn registered_provider_resolves_and_passes_its_uri_through_verbatim() {
        let s = store();
        s.register_machine_provider("incredibuild", "build farm", "/opt/ib.sh", &[], 1, false)
            .unwrap();
        assert_eq!(
            s.resolve_machine(Some("incredibuild:A")).unwrap(),
            ResolvedMachine::Provider {
                scheme: "incredibuild".to_string(),
                uri: "A".to_string(),
            }
        );
        // The uri half is opaque -- a URL with its own colons survives intact.
        assert_eq!(
            s.resolve_machine(Some("incredibuild:https://useful.com/x"))
                .unwrap(),
            ResolvedMachine::Provider {
                scheme: "incredibuild".to_string(),
                uri: "https://useful.com/x".to_string(),
            }
        );
    }

    #[test]
    fn scheme_matching_is_case_insensitive() {
        let s = store();
        s.register_machine_provider("IncrediBuild", "", "/opt/ib.sh", &[], 1, false)
            .unwrap();
        assert!(matches!(
            s.resolve_machine(Some("INCREDIBUILD:A")).unwrap(),
            ResolvedMachine::Provider { .. }
        ));
    }

    #[test]
    fn unknown_scheme_errors_and_lists_what_is_available() {
        let s = store();
        s.register_machine_provider("incredibuild", "", "/opt/ib.sh", &[], 1, false)
            .unwrap();
        let err = s
            .resolve_machine(Some("nope:A"))
            .expect_err("unregistered scheme must fail");
        let ResolveError::UnknownScheme { scheme, known } = &err else {
            panic!("expected UnknownScheme, got {err:?}");
        };
        assert_eq!(scheme, "nope");
        assert!(known.contains(&"incredibuild".to_string()));
        assert!(
            known.contains(&"local".to_string()),
            "the built-in must be offered too: {known:?}"
        );
        // The message is what the user actually sees at submit time.
        assert!(err.to_string().contains("incredibuild"), "{err}");
    }

    #[test]
    fn malformed_machine_errors_before_touching_the_registry() {
        let s = store();
        assert!(matches!(
            s.resolve_machine(Some("incredibuild")),
            Err(ResolveError::Malformed(_))
        ));
    }

    #[test]
    fn provider_with_a_mismatched_contract_version_is_refused_not_invoked() {
        let s = store();
        s.register_machine_provider("old", "", "/opt/old.sh", &[], PROTOCOL_VERSION + 1, false)
            .unwrap();
        let err = s.resolve_machine(Some("old:A")).expect_err("must refuse");
        assert!(matches!(err, ResolveError::UnsupportedProtocol { .. }));
        assert!(err.to_string().contains("contract version"), "{err}");
    }

    #[test]
    fn register_upserts_and_deregister_removes() {
        let s = store();
        s.register_machine_provider("ib", "first", "/a.sh", &[], 1, false)
            .unwrap();
        s.register_machine_provider("ib", "second", "/b.sh", &["--flavor".to_string()], 1, false)
            .unwrap();
        let all = s.list_machine_providers().unwrap();
        assert_eq!(all.len(), 1, "re-registering must upsert, not duplicate");
        assert_eq!(all[0].description, "second");
        assert_eq!(all[0].program, "/b.sh");
        assert_eq!(all[0].args, vec!["--flavor".to_string()]);

        assert!(s.deregister_machine_provider("ib").unwrap());
        assert!(
            !s.deregister_machine_provider("ib").unwrap(),
            "second delete reports nothing removed"
        );
        assert!(s.list_machine_providers().unwrap().is_empty());
    }

    #[test]
    fn a_deregistered_scheme_fails_a_new_resolution() {
        let s = store();
        s.register_machine_provider("ib", "", "/a.sh", &[], 1, false)
            .unwrap();
        assert!(s.resolve_machine(Some("ib:A")).is_ok());
        s.deregister_machine_provider("ib").unwrap();
        assert!(matches!(
            s.resolve_machine(Some("ib:A")),
            Err(ResolveError::UnknownScheme { .. })
        ));
    }

    /// RAL-200: the ssh machine provider registers under the plain "ssh"
    /// scheme through this same generic mechanism -- no new daemon-side
    /// plumbing needed. This pins that "ssh" collides with neither the
    /// built-in `local` scheme nor the `ralphus` reserved word, and that
    /// re-registering it (e.g. after a path change or a version bump)
    /// upserts cleanly rather than duplicating, exactly like every other
    /// provider.
    #[test]
    fn the_ssh_scheme_is_registrable_and_re_registration_upserts() {
        assert!(
            !is_unregistrable_scheme("ssh"),
            "\"ssh\" must not collide with a built-in or reserved scheme"
        );
        let s = store();
        s.register_machine_provider(
            "ssh",
            "reaches any host already reachable via ssh (RAL-200)",
            "/opt/ralphus/ralphus-ssh-provider",
            &[],
            PROTOCOL_VERSION,
            false,
        )
        .unwrap();
        assert_eq!(
            s.resolve_machine(Some("ssh:alice@build-box")).unwrap(),
            ResolvedMachine::Provider {
                scheme: "ssh".to_string(),
                uri: "alice@build-box".to_string(),
            }
        );

        // Re-registering (e.g. a path change after a release) upserts.
        s.register_machine_provider(
            "ssh",
            "reaches any host already reachable via ssh (RAL-200)",
            "/opt/ralphus/v2/ralphus-ssh-provider",
            &[],
            PROTOCOL_VERSION,
            false,
        )
        .unwrap();
        let all = s.list_machine_providers().unwrap();
        assert_eq!(
            all.iter().filter(|p| p.scheme == "ssh").count(),
            1,
            "re-registering \"ssh\" must upsert, not duplicate"
        );
        assert_eq!(
            s.get_machine_provider("ssh").unwrap().unwrap().program,
            "/opt/ralphus/v2/ralphus-ssh-provider"
        );
    }
}
