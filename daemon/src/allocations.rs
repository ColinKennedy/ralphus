//! The client side of a provider service's allocation API (contract v2) --
//! see [`docs/machine-provider-service.md`](../../docs/machine-provider-service.md).
//!
//! Ralphus asks a provider service for a machine that satisfies a request and
//! either gets an [`Allocation`] or a [`ServiceError`] saying why not. Which
//! machine, and how it is reached, is the provider's business: an allocation
//! is only an endpoint and a token for its Machine API
//! ([`crate::remote_runner::ProviderRunner::over_http`]).

use std::io::Read as _;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use rusqlite::OptionalExtension as _;

use crate::machines::SERVICE_PROTOCOL_VERSION;
use crate::provider_services::ProviderService;
use crate::store::{Result as StoreResult, Store, now_ms};

/// One machine a provider service handed out.
#[derive(Clone, PartialEq, Eq)]
pub struct Allocation {
    pub allocation_id: String,
    /// The allocation's Machine API base URL.
    pub endpoint: String,
    /// Bearer token for [`Self::endpoint`]. Short-lived; never logged or
    /// written to the store.
    pub token: String,
    pub token_expires_at_ms: Option<i64>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub labels: Vec<String>,
    /// The provider's human-readable name for the machine.
    pub description: Option<String>,
}

impl std::fmt::Debug for Allocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Allocation")
            .field("allocation_id", &self.allocation_id)
            .field("endpoint", &self.endpoint)
            .field("token", &"<redacted>")
            .field("token_expires_at_ms", &self.token_expires_at_ms)
            .field("os", &self.os)
            .field("arch", &self.arch)
            .field("labels", &self.labels)
            .field("description", &self.description)
            .finish()
    }
}

/// A provider service refusing, or failing to answer, a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceError {
    pub provider: String,
    /// The contract's classification (`no_matching_machine`, `capacity`,
    /// `unreachable`, `unauthorized`, `unknown_allocation`, `unsupported`),
    /// or `unavailable` when the daemon could not call the provider at all.
    pub reason_code: String,
    pub message: String,
    /// What the user should do, shown verbatim.
    pub remediation: Option<String>,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider {:?} [{}]: {}",
            self.provider, self.reason_code, self.message
        )?;
        if let Some(remediation) = &self.remediation {
            write!(f, "; {remediation}")?;
        }
        Ok(())
    }
}

/// `reason_code` for a provider the daemon could not call: its token is
/// unset, or it did not answer.
pub const REASON_UNAVAILABLE: &str = "unavailable";
/// `reason_code` for an allocation id the provider does not know.
pub const REASON_UNKNOWN_ALLOCATION: &str = "unknown_allocation";

/// Every field a provider service reply may carry.
#[derive(Debug, Default, Deserialize)]
struct Reply {
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    protocol_version: Option<i64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    reason_code: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
    #[serde(default)]
    allocation_id: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    token_expires_at_ms: Option<i64>,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    arch: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    description: Option<String>,
}

fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::LazyLock<ureq::Agent> =
        std::sync::LazyLock::new(|| crate::forge::bounded_http_agent(Duration::from_secs(120)));
    &AGENT
}

fn unavailable(service: &ProviderService, message: String) -> ServiceError {
    ServiceError {
        provider: service.name.clone(),
        reason_code: REASON_UNAVAILABLE.to_string(),
        message,
        remediation: Some(format!(
            "check that provider service {:?} is running at {} and reachable from the daemon \
             host",
            service.name, service.url
        )),
    }
}

/// Call one provider service endpoint and return its checked reply.
fn call(
    service: &ProviderService,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Reply, ServiceError> {
    let token = service.token().map_err(|message| ServiceError {
        provider: service.name.clone(),
        reason_code: REASON_UNAVAILABLE.to_string(),
        message,
        remediation: Some(format!(
            "set {} in the daemon's environment and restart the daemon",
            service.token_env
        )),
    })?;
    let url = format!("{}{path}", service.url);
    let request = agent()
        .request(method, &url)
        .set("Authorization", &format!("Bearer {token}"));
    let sent = match body {
        Some(body) => request
            .set("Content-Type", "application/json")
            .send_string(&body.to_string()),
        None => request.call(),
    };
    let (status, response) = match sent {
        Ok(response) => (response.status(), response),
        Err(ureq::Error::Status(code, response)) => (code, response),
        Err(e) => return Err(unavailable(service, format!("{method} {url} failed: {e}"))),
    };
    let mut text = String::new();
    response
        .into_reader()
        .take(1024 * 1024)
        .read_to_string(&mut text)
        .map_err(|e| unavailable(service, format!("{method} {url}: unreadable reply: {e}")))?;
    let reply: Reply = if text.trim().is_empty() {
        Reply::default()
    } else {
        serde_json::from_str(text.trim()).map_err(|e| {
            unavailable(
                service,
                format!(
                    "{method} {url} answered HTTP {status} with unparseable JSON: {e} ({})",
                    crate::redact::redact_all(&text.chars().take(300).collect::<String>())
                ),
            )
        })?
    };
    if status >= 400 || reply.ok == Some(false) {
        let fallback = if status == 404 {
            REASON_UNKNOWN_ALLOCATION
        } else {
            REASON_UNAVAILABLE
        };
        return Err(ServiceError {
            provider: service.name.clone(),
            reason_code: reply.reason_code.unwrap_or_else(|| fallback.to_string()),
            message: reply
                .error
                .unwrap_or_else(|| format!("{method} {path} answered HTTP {status}")),
            remediation: reply.remediation,
        });
    }
    if let Some(version) = reply
        .protocol_version
        .filter(|v| *v != SERVICE_PROTOCOL_VERSION)
    {
        return Err(ServiceError {
            provider: service.name.clone(),
            reason_code: "unsupported".to_string(),
            message: format!(
                "replied with contract version {version}, but this daemon implements \
                 {SERVICE_PROTOCOL_VERSION}"
            ),
            remediation: Some(format!(
                "upgrade provider service {:?} or the daemon so both speak contract version \
                 {SERVICE_PROTOCOL_VERSION}",
                service.name
            )),
        });
    }
    Ok(reply)
}

fn allocation_from(service: &ProviderService, reply: Reply) -> Result<Allocation, ServiceError> {
    let missing = |field: &str| {
        unavailable(
            service,
            format!(
                "the allocation reply has no {field}; the provider does not follow contract version {SERVICE_PROTOCOL_VERSION}"
            ),
        )
    };
    Ok(Allocation {
        allocation_id: reply
            .allocation_id
            .ok_or_else(|| missing("allocation_id"))?,
        endpoint: reply
            .endpoint
            .ok_or_else(|| missing("endpoint"))?
            .trim_end_matches('/')
            .to_string(),
        token: reply.token.ok_or_else(|| missing("token"))?,
        token_expires_at_ms: reply.token_expires_at_ms,
        os: reply.os,
        arch: reply.arch,
        labels: reply.labels,
        description: reply.description,
    })
}

/// The requirements object of a `POST /v1/allocations` request.
fn requirements(request: &ralphus_core::schema::MachineRequest) -> Value {
    let mut fields = serde_json::Map::new();
    if let Some(os) = &request.os {
        fields.insert("os".into(), os.clone().into());
    }
    if let Some(arch) = &request.arch {
        fields.insert("arch".into(), arch.clone().into());
    }
    if !request.labels.is_empty() {
        fields.insert("labels".into(), request.labels.clone().into());
    }
    Value::Object(fields)
}

/// Whose work an allocation serves: a squad's tasks, cells and proofs, or a
/// review that asked for a machine of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    Squad(&'a str),
    Review(&'a str),
}

impl Scope<'_> {
    fn squad_id(self) -> Option<String> {
        match self {
            Self::Squad(id) => Some(id.to_string()),
            Self::Review(_) => None,
        }
    }

    fn review_id(self) -> Option<String> {
        match self {
            Self::Squad(_) => None,
            Self::Review(id) => Some(id.to_string()),
        }
    }
}

/// Ask `service` for a machine satisfying `request`, for the work of `scope`
/// that shares the request `key`.
///
/// # Errors
/// The provider's refusal, or [`REASON_UNAVAILABLE`] when it could not be
/// asked.
pub fn allocate(
    service: &ProviderService,
    request: &ralphus_core::schema::MachineRequest,
    scope: Scope<'_>,
    key: &str,
) -> Result<Allocation, ServiceError> {
    let mut scope_fields = serde_json::Map::new();
    if let Some(id) = scope.squad_id() {
        scope_fields.insert("squad_id".into(), id.into());
    }
    if let Some(id) = scope.review_id() {
        scope_fields.insert("review_id".into(), id.into());
    }
    scope_fields.insert("key".into(), key.into());
    let body = json!({
        "requirements": requirements(request),
        "scope": scope_fields,
    });
    let reply = call(service, "POST", "/v1/allocations", Some(&body))?;
    allocation_from(service, reply)
}

/// Re-read an allocation, e.g. after a daemon restart.
///
/// # Errors
/// [`REASON_UNKNOWN_ALLOCATION`] when the provider no longer knows it.
pub fn get(service: &ProviderService, allocation_id: &str) -> Result<Allocation, ServiceError> {
    let reply = call(
        service,
        "GET",
        &format!("/v1/allocations/{allocation_id}"),
        None,
    )?;
    allocation_from(service, reply)
}

/// Get a fresh token for an allocation.
///
/// # Errors
/// As [`get`].
pub fn renew(service: &ProviderService, allocation_id: &str) -> Result<Allocation, ServiceError> {
    let reply = call(
        service,
        "POST",
        &format!("/v1/allocations/{allocation_id}/renew"),
        Some(&json!({})),
    )?;
    allocation_from(service, reply)
}

/// Tell the provider whether the allocation is waiting on a human. A hint:
/// the provider may act on it, and must resume on the next Machine API call.
///
/// # Errors
/// As [`get`].
pub fn set_idle(
    service: &ProviderService,
    allocation_id: &str,
    idle: bool,
) -> Result<(), ServiceError> {
    call(
        service,
        "POST",
        &format!("/v1/allocations/{allocation_id}/idle"),
        Some(&json!({"idle": idle})),
    )
    .map(|_| ())
}

/// Hand an allocation back.
///
/// # Errors
/// As [`get`].
pub fn release(service: &ProviderService, allocation_id: &str) -> Result<(), ServiceError> {
    call(
        service,
        "DELETE",
        &format!("/v1/allocations/{allocation_id}"),
        None,
    )
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// Bound machine values
// ---------------------------------------------------------------------------

/// Separates a request from the allocation row it was granted as, in a bound
/// machine value: `@fleet?os=linux#12`. A task file cannot contain one (a
/// provider name has no `#`), so only the daemon writes them.
pub const BOUND_SEPARATOR: char = '#';

/// The machine value naming allocation row `row_id`, granted for `request`.
#[must_use]
pub fn bound_machine(request: &str, row_id: i64) -> String {
    format!("{request}{BOUND_SEPARATOR}{row_id}")
}

/// `(request, allocation row id)` of a bound machine value.
#[must_use]
pub fn parse_bound(machine: &str) -> Option<(&str, i64)> {
    let machine = machine.trim();
    if !machine.starts_with(ralphus_core::schema::MACHINE_REQUEST_PREFIX) {
        return None;
    }
    let (request, row) = machine.rsplit_once(BOUND_SEPARATOR)?;
    Some((request, row.parse().ok()?))
}

/// Whether `machine` is a provider-service request no allocation has been
/// granted for yet.
#[must_use]
pub fn is_unbound_request(machine: &str) -> bool {
    let machine = machine.trim();
    machine.starts_with(ralphus_core::schema::MACHINE_REQUEST_PREFIX)
        && parse_bound(machine).is_none()
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// `machine_allocations.state`: the allocation is in use.
pub const STATE_ACTIVE: &str = "active";
/// `machine_allocations.state`: handed back to its provider.
pub const STATE_RELEASED: &str = "released";
/// `machine_allocations.state`: its provider no longer knows it.
pub const STATE_LOST: &str = "lost";

/// One `machine_allocations` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AllocationRow {
    pub id: i64,
    pub provider: String,
    pub allocation_id: String,
    pub endpoint: String,
    /// The canonical request it was granted for.
    pub request: String,
    pub squad_id: Option<String>,
    pub guardian_id: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub labels: Vec<String>,
    pub description: Option<String>,
    pub idle: bool,
    pub state: String,
    pub state_detail: Option<String>,
    pub created_at_ms: i64,
    pub released_at_ms: Option<i64>,
}

impl AllocationRow {
    /// The machine value that names this allocation.
    #[must_use]
    pub fn machine(&self) -> String {
        bound_machine(&self.request, self.id)
    }
}

const ALLOCATION_COLUMNS: &str = "id, provider, allocation_id, endpoint, request, squad_id, \
     guardian_id, os, arch, labels, description, idle, state, state_detail, created_at_ms, \
     released_at_ms";

fn allocation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AllocationRow> {
    let labels: String = row.get(9)?;
    Ok(AllocationRow {
        id: row.get(0)?,
        provider: row.get(1)?,
        allocation_id: row.get(2)?,
        endpoint: row.get(3)?,
        request: row.get(4)?,
        squad_id: row.get(5)?,
        guardian_id: row.get(6)?,
        os: row.get(7)?,
        arch: row.get(8)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        description: row.get(10)?,
        idle: row.get::<_, i64>(11)? != 0,
        state: row.get(12)?,
        state_detail: row.get(13)?,
        created_at_ms: row.get(14)?,
        released_at_ms: row.get(15)?,
    })
}

impl Store {
    /// Record an allocation `provider` granted for `request`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn insert_machine_allocation(
        &self,
        provider: &str,
        allocation: &Allocation,
        request: &str,
        scope: Scope<'_>,
    ) -> StoreResult<i64> {
        self.conn.execute(
            "INSERT INTO machine_allocations(provider, allocation_id, endpoint, request, \
             squad_id, guardian_id, os, arch, labels, description, created_at_ms) \
             VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            rusqlite::params![
                provider,
                allocation.allocation_id,
                allocation.endpoint,
                request,
                scope.squad_id(),
                scope.review_id(),
                allocation.os,
                allocation.arch,
                serde_json::to_string(&allocation.labels).unwrap_or_else(|_| "[]".to_string()),
                allocation.description,
                now_ms(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// One allocation row.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn machine_allocation(&self, id: i64) -> StoreResult<Option<AllocationRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ALLOCATION_COLUMNS} FROM machine_allocations WHERE id=?"),
                [id],
                allocation_from_row,
            )
            .optional()?)
    }

    /// Every allocation, newest first; only `active` ones unless `all`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn machine_allocations(&self, all: bool) -> StoreResult<Vec<AllocationRow>> {
        let filter = if all { "" } else { "WHERE state='active'" };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ALLOCATION_COLUMNS} FROM machine_allocations {filter} ORDER BY id DESC"
        ))?;
        let rows = stmt
            .query_map([], allocation_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Mark an allocation `released` or `lost`, with why.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_machine_allocation_state(
        &self,
        id: i64,
        state: &str,
        detail: Option<&str>,
    ) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE machine_allocations SET state=?, state_detail=?, \
             released_at_ms=CASE WHEN ?='active' THEN NULL ELSE ? END WHERE id=?",
            rusqlite::params![state, detail, state, now_ms(), id],
        )?;
        Ok(())
    }

    /// Record the idle hint last sent for an allocation.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn set_machine_allocation_idle(&self, id: i64, idle: bool) -> StoreResult<()> {
        self.conn.execute(
            "UPDATE machine_allocations SET idle=? WHERE id=?",
            rusqlite::params![i64::from(idle), id],
        )?;
        Ok(())
    }

    /// Point every cell of `squad_id` whose machine is `from` at `to`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn rebind_squad_machine(&self, squad_id: &str, from: &str, to: &str) -> StoreResult<usize> {
        Ok(self.conn.execute(
            "UPDATE cells SET machine=? WHERE squad_id=? AND machine=?",
            rusqlite::params![to, squad_id, from],
        )?)
    }
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// Renew a token this long before it expires.
const RENEW_MARGIN_MS: i64 = 60_000;

struct CachedToken {
    token: String,
    expires_at_ms: Option<i64>,
}

/// Allocation tokens by row id. In memory only: after a restart each is
/// renewed on first use, so no token is ever at rest.
static TOKENS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<i64, CachedToken>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn remember_token(row_id: i64, allocation: &Allocation) {
    TOKENS.lock().expect("poisoned").insert(
        row_id,
        CachedToken {
            token: allocation.token.clone(),
            expires_at_ms: allocation.token_expires_at_ms,
        },
    );
}

/// A current token for allocation row `row`, renewed from `service` when
/// there is none or it is about to expire.
///
/// # Errors
/// The provider's refusal to renew, with its remediation.
pub fn token_for(row: &AllocationRow, service: &ProviderService) -> Result<String, String> {
    {
        let tokens = TOKENS.lock().expect("poisoned");
        if let Some(cached) = tokens.get(&row.id) {
            let fresh = cached
                .expires_at_ms
                .is_none_or(|at| at - RENEW_MARGIN_MS > now_ms());
            if fresh {
                return Ok(cached.token.clone());
            }
        }
    }
    let renewed = renew(service, &row.allocation_id).map_err(|e| {
        format!(
            "allocation {} of machine {} could not be renewed: {e}",
            row.allocation_id,
            row.machine()
        )
    })?;
    remember_token(row.id, &renewed);
    Ok(renewed.token)
}

// ---------------------------------------------------------------------------
// Granting
// ---------------------------------------------------------------------------

/// Why the daemon's own host cannot serve `request`, or `None` when it can.
#[must_use]
pub fn local_refusal(request: &ralphus_core::schema::MachineRequest) -> Option<String> {
    let host_os = std::env::consts::OS;
    let host_arch = std::env::consts::ARCH;
    if let Some(os) = request.os.as_deref().filter(|os| *os != host_os) {
        return Some(format!("the daemon host runs {host_os}, not {os}"));
    }
    if let Some(arch) = request.arch.as_deref().filter(|arch| *arch != host_arch) {
        return Some(format!("the daemon host is {host_arch}, not {arch}"));
    }
    if !request.labels.is_empty() {
        return Some("the daemon host carries no labels".to_string());
    }
    None
}

/// Walk the providers `request_text` names (or, for
/// `<<ralphus:auto-provider>>`, every configured provider service in
/// declaration order) and return the machine value of the first grant:
/// `local`, or a value bound to a new allocation row.
///
/// # Errors
/// Every provider tried, with its refusal and remediation.
pub fn grant(
    store: &crate::store_lock::StoreHandle,
    request_text: &str,
    scope: Scope<'_>,
) -> Result<String, String> {
    let request = ralphus_core::schema::MachineRequest::parse_canonical(request_text)
        .ok_or_else(|| format!("machine {request_text:?} is not a provider request"))?;
    let services = crate::provider_services::load_provider_services()?;
    let candidates: Vec<String> = if request.is_auto() {
        services.iter().map(|s| s.name.clone()).collect()
    } else {
        request.providers.clone()
    };
    let mut refusals = Vec::new();
    for name in &candidates {
        if name.eq_ignore_ascii_case(ralphus_core::schema::LOCAL_MACHINE) {
            match local_refusal(&request) {
                None => return Ok(ralphus_core::schema::LOCAL_MACHINE.to_string()),
                Some(reason) => refusals.push(format!("local: {reason}")),
            }
            continue;
        }
        let Some(service) = services.iter().find(|s| s.name == *name) else {
            refusals.push(format!(
                "{name}: not configured; declare [machine.providers.{name}] in the daemon's \
                 config.toml"
            ));
            continue;
        };
        match allocate(service, &request, scope, request_text) {
            Ok(allocation) => {
                let row_id = store
                    .lock()
                    .insert_machine_allocation(name, &allocation, request_text, scope)
                    .map_err(|e| format!("could not record the allocation: {e}"))?;
                remember_token(row_id, &allocation);
                let machine = bound_machine(request_text, row_id);
                crate::rlog!(
                    INFO,
                    "ralphus [allocations] provider {name} granted {request_text} for {scope:?} \
                     as allocation {} ({})",
                    allocation.allocation_id,
                    allocation
                        .description
                        .as_deref()
                        .unwrap_or("no description")
                );
                let _ = store
                    .lock()
                    .cartographer_log(crate::cartographer::CartographerEntry {
                        level: crate::logging::LogLevel::INFO,
                        source: "allocations",
                        message: "machine allocated",
                        scope: Some(match scope {
                            Scope::Squad(_) => "squad",
                            Scope::Review(_) => "guardian",
                        }),
                        squad_id: scope.squad_id().as_deref(),
                        guardian_id: scope.review_id().as_deref(),
                        cell_id: None,
                        task: None,
                        log_path: None,
                        payload: json!({
                            "provider": name,
                            "allocation_id": allocation.allocation_id,
                            "request": request_text,
                            "machine": machine,
                            "description": allocation.description,
                        }),
                        admin_only: false,
                    });
                return Ok(machine);
            }
            Err(e) => refusals.push(e.to_string()),
        }
    }
    if candidates.is_empty() {
        refusals.push(
            "no provider service is configured; declare one under [machine.providers] in the \
             daemon's config.toml"
                .to_string(),
        );
    }
    Err(format!(
        "no provider granted machine {request_text}: {}",
        refusals.join("; ")
    ))
}

/// Grant every provider request among `squad_id`'s cells that has no live
/// allocation, and rewrite those cells (in the store and in `cells`) to the
/// granted machine. Cells with an equal request share one allocation.
///
/// # Errors
/// The first request no provider granted.
pub fn bind_squad(
    store: &crate::store_lock::StoreHandle,
    squad_id: &str,
    cells: &mut [crate::store::CellRow],
) -> Result<(), String> {
    let mut pending: Vec<String> = Vec::new();
    for machine in cells.iter().filter_map(|c| c.machine.as_deref()) {
        let needs_grant = match parse_bound(machine) {
            None => is_unbound_request(machine),
            // A released or lost allocation (a restarted squad) is asked for
            // again.
            Some((_, row)) => store
                .lock()
                .machine_allocation(row)
                .ok()
                .flatten()
                .is_none_or(|r| r.state != STATE_ACTIVE),
        };
        if needs_grant && !pending.iter().any(|m| m == machine) {
            pending.push(machine.to_string());
        }
    }
    for current in pending {
        let request = parse_bound(&current).map_or(current.as_str(), |(r, _)| r);
        let granted = grant(store, request, Scope::Squad(squad_id))?;
        store
            .lock()
            .rebind_squad_machine(squad_id, &current, &granted)
            .map_err(|e| format!("could not record machine {granted} on the squad: {e}"))?;
        for cell in cells
            .iter_mut()
            .filter(|c| c.machine.as_deref() == Some(current.as_str()))
        {
            cell.machine = Some(granted.clone());
        }
    }
    Ok(())
}

/// How long a review whose request no provider granted waits before asking
/// again, so a review touched many times a minute does not ask each time.
const REVIEW_RETRY_MS: i64 = 60_000;

/// Reviews whose last grant failed: guardian id → (when, why).
static REVIEW_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (i64, String)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Grant a review's own provider request (its `[[review]] machine` or the
/// project default) and record the granted machine on the review. An active
/// allocation one of its squads holds for the same request is reused, so the
/// review merges where those cells ran.
///
/// # Errors
/// Why no provider granted it; repeated without asking again for
/// [`REVIEW_RETRY_MS`] after a failure.
pub fn bind_review(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    request: &str,
) -> Result<String, String> {
    if let Some((at, reason)) = REVIEW_FAILURES.lock().expect("poisoned").get(guardian_id) {
        if now_ms() - at < REVIEW_RETRY_MS {
            return Err(reason.clone());
        }
    }
    let reuse = {
        let guard = store.lock();
        let guardian = guard
            .get_guardian(guardian_id)
            .map_err(|e| format!("could not load review {guardian_id}: {e}"))?;
        let mut squads: Vec<String> = guardian.squad_id.into_iter().collect();
        squads.extend(
            guardian
                .branches
                .iter()
                .filter_map(|b| b.source_squad_id.clone()),
        );
        guard
            .machine_allocations(false)
            .unwrap_or_default()
            .into_iter()
            .find(|row| {
                row.request == request && row.squad_id.as_ref().is_some_and(|s| squads.contains(s))
            })
    };
    let granted = match reuse {
        Some(row) => Ok(row.machine()),
        None => grant(store, request, Scope::Review(guardian_id)),
    };
    let machine = match granted {
        Ok(machine) => machine,
        Err(reason) => {
            REVIEW_FAILURES
                .lock()
                .expect("poisoned")
                .insert(guardian_id.to_string(), (now_ms(), reason.clone()));
            return Err(reason);
        }
    };
    REVIEW_FAILURES
        .lock()
        .expect("poisoned")
        .remove(guardian_id);
    store
        .lock()
        .set_guardian_machine(guardian_id, Some(&machine))
        .map_err(|e| format!("could not record machine {machine} on review {guardian_id}: {e}"))?;
    Ok(machine)
}

/// The request to grant for `machine` when it has no live allocation: an
/// unbound request, or one bound to an allocation since released or lost.
#[must_use]
pub fn request_needing_grant(
    store: &crate::store_lock::StoreHandle,
    machine: &str,
) -> Option<String> {
    if is_unbound_request(machine) {
        return Some(machine.trim().to_string());
    }
    let (request, row) = parse_bound(machine)?;
    let active = store
        .lock()
        .machine_allocation(row)
        .ok()
        .flatten()
        .is_some_and(|r| r.state == STATE_ACTIVE);
    (!active).then(|| request.to_string())
}

// ---------------------------------------------------------------------------
// Maintenance: re-attach, release, idle
// ---------------------------------------------------------------------------

/// Review statuses in which a review is waiting on a human, so its machine
/// may idle.
const WAITING_ON_HUMAN: [&str; 3] = ["in_review", "merge_failed", "merge_stopped"];

impl Store {
    /// `(id, status)` of every review recorded on `machine`.
    ///
    /// # Errors
    /// Propagates any SQLite failure.
    pub fn guardians_on_machine(&self, machine: &str) -> StoreResult<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, status FROM guardians WHERE machine=?")?;
        let rows = stmt
            .query_map([machine], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Whether anything still needs allocation `row`, and whether all of it is
/// waiting on a human.
fn allocation_use(store: &Store, row: &AllocationRow) -> (bool, bool) {
    let squad_busy = row.squad_id.as_deref().is_some_and(|squad| {
        store
            .squad_state(squad)
            .is_ok_and(|state| !state.is_terminal())
    });
    let reviews: Vec<String> = store
        .guardians_on_machine(&row.machine())
        .unwrap_or_default()
        .into_iter()
        .map(|(_, status)| status)
        .filter(|status| !crate::guardian::GuardianStatus::is_terminal_status(status))
        .collect();
    let needed = squad_busy || !reviews.is_empty();
    let idle = needed
        && !squad_busy
        && reviews
            .iter()
            .all(|status| WAITING_ON_HUMAN.contains(&status.as_str()));
    (needed, idle)
}

fn note(
    store: &crate::store_lock::StoreHandle,
    row: &AllocationRow,
    level: crate::logging::LogLevel,
    message: &str,
) {
    crate::rlog!(
        INFO,
        "ralphus [allocations] {message}: allocation {} of provider {} ({})",
        row.allocation_id,
        row.provider,
        row.machine()
    );
    let _ = store
        .lock()
        .cartographer_log(crate::cartographer::CartographerEntry {
            level,
            source: "allocations",
            message,
            scope: row.squad_id.as_ref().map(|_| "squad").or(Some("guardian")),
            squad_id: row.squad_id.as_deref(),
            guardian_id: row.guardian_id.as_deref(),
            cell_id: None,
            task: None,
            log_path: None,
            payload: json!({
                "provider": row.provider,
                "allocation_id": row.allocation_id,
                "machine": row.machine(),
            }),
            admin_only: false,
        });
}

/// Whether [`maintain`] has re-read every allocation since the daemon
/// started.
static REATTACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One pass over the active allocations. The first pass after a start
/// re-reads each from its provider and marks the ones it no longer knows
/// lost (with a mailbox error); every pass hands back the ones nothing needs
/// any more and tells providers which are waiting on a human. Network calls
/// happen with no store lock held.
pub fn maintain(store: &crate::store_lock::StoreHandle) {
    let rows = store.lock().machine_allocations(false).unwrap_or_default();
    let reattach = !REATTACHED.swap(true, std::sync::atomic::Ordering::SeqCst);
    if rows.is_empty() {
        return;
    }
    let services = match crate::provider_services::load_provider_services() {
        Ok(services) => services,
        Err(e) => {
            crate::rlog!(
                WARNING,
                "ralphus [allocations] could not load provider services: {e}"
            );
            return;
        }
    };
    for row in rows {
        let Some(service) = services.iter().find(|s| s.name == row.provider) else {
            continue;
        };
        if reattach {
            if let Err(e) = get(service, &row.allocation_id) {
                if e.reason_code == REASON_UNKNOWN_ALLOCATION {
                    mark_lost(store, &row, &e);
                    continue;
                }
            }
        }
        let (needed, idle) = allocation_use(&store.lock(), &row);
        if !needed {
            match release(service, &row.allocation_id) {
                Ok(()) => {}
                Err(e) if e.reason_code == REASON_UNKNOWN_ALLOCATION => {}
                Err(e) => {
                    crate::rlog!(
                        WARNING,
                        "ralphus [allocations] could not release allocation {}: {e}",
                        row.allocation_id
                    );
                    continue;
                }
            }
            let _ = store
                .lock()
                .set_machine_allocation_state(row.id, STATE_RELEASED, None);
            TOKENS.lock().expect("poisoned").remove(&row.id);
            note(
                store,
                &row,
                crate::logging::LogLevel::INFO,
                "machine released",
            );
            continue;
        }
        if idle != row.idle && set_idle(service, &row.allocation_id, idle).is_ok() {
            let _ = store.lock().set_machine_allocation_idle(row.id, idle);
        }
    }
}

/// Run [`maintain`] on its own thread unless a pass is already running, so a
/// slow provider never holds up the scheduler loop that calls this.
pub fn maintain_in_background(store: &crate::store_lock::StoreHandle) {
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let store = std::sync::Arc::clone(store);
    std::thread::spawn(move || {
        maintain(&store);
        RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

fn mark_lost(store: &crate::store_lock::StoreHandle, row: &AllocationRow, error: &ServiceError) {
    let detail = error.to_string();
    let guard = store.lock();
    let _ = guard.set_machine_allocation_state(row.id, STATE_LOST, Some(&detail));
    TOKENS.lock().expect("poisoned").remove(&row.id);
    let restart = row.squad_id.as_ref().map_or_else(
        || {
            format!(
                "reopen or restart review {} so it is granted a new machine",
                row.guardian_id.as_deref().unwrap_or("?")
            )
        },
        |squad| format!("run `ralphus squad restart {squad}` so it is granted a new machine"),
    );
    let _ = guard.enqueue_error_mailbox_message(
        crate::mailbox::MailboxPriority::High,
        &format!(
            "Machine {} (allocation {} from provider {:?}) is gone: {detail}",
            row.machine(),
            row.allocation_id,
            row.provider
        ),
        &crate::mailbox::Remediation::ManualInterventionRequired { guidance: restart },
        row.squad_id.as_deref(),
        None,
        None,
        None,
        Some("machine_allocation"),
    );
    drop(guard);
    note(
        store,
        row,
        crate::logging::LogLevel::WARNING,
        "machine lost",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(method, url, authorization header, body)` of one stub request.
    type Seen = (String, String, String, Value);

    /// A provider service stand-in answering one request with `(status,
    /// body)`, plus a configured [`ProviderService`] pointing at it.
    fn service(
        tag: &str,
        status: u16,
        reply: Value,
    ) -> (ProviderService, std::thread::JoinHandle<Seen>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let token_env = format!("RALPHUS_TEST_ALLOC_{}", tag.to_uppercase());
        crate::provider_services::set_token_for_test(&token_env, "svc-token");
        let svc = ProviderService {
            name: "fleet".to_string(),
            url: format!("http://{}", server.server_addr()),
            token_env,
        };
        let handle = std::thread::spawn(move || {
            let mut req = server
                .recv_timeout(Duration::from_secs(30))
                .unwrap()
                .expect("a request");
            let auth = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .map(|h| h.value.to_string())
                .unwrap_or_default();
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let seen = (
                req.method().to_string(),
                req.url().to_string(),
                auth,
                serde_json::from_str(&body).unwrap_or(Value::Null),
            );
            let _ = req.respond(
                tiny_http::Response::from_string(reply.to_string()).with_status_code(status),
            );
            seen
        });
        (svc, handle)
    }

    fn linux_gpu() -> ralphus_core::schema::MachineRequest {
        ralphus_core::schema::MachineRequest {
            providers: vec!["fleet".to_string()],
            os: Some("linux".to_string()),
            arch: None,
            labels: vec!["gpu".to_string()],
        }
    }

    #[test]
    fn allocate_sends_requirements_and_scope_and_reads_the_allocation() {
        let (svc, server) = service(
            "grant",
            200,
            json!({
                "protocol_version": 2,
                "allocation_id": "alloc-7f3a",
                "endpoint": "http://127.0.0.1:7900/v1/m/alloc-7f3a/",
                "token": "machine-token",
                "os": "linux",
                "labels": ["gpu"],
                "description": "mybox/wsl",
            }),
        );

        let allocation = allocate(
            &svc,
            &linux_gpu(),
            Scope::Squad("squad-1"),
            "@fleet?os=linux&labels=gpu",
        )
        .expect("granted");

        assert_eq!(allocation.allocation_id, "alloc-7f3a");
        assert_eq!(allocation.endpoint, "http://127.0.0.1:7900/v1/m/alloc-7f3a");
        assert_eq!(allocation.token, "machine-token");
        assert!(!format!("{allocation:?}").contains("machine-token"));
        let (method, url, auth, body) = server.join().unwrap();
        assert_eq!((method.as_str(), url.as_str()), ("POST", "/v1/allocations"));
        assert_eq!(auth, "Bearer svc-token");
        assert_eq!(
            body,
            json!({
                "requirements": {"os": "linux", "labels": ["gpu"]},
                "scope": {"squad_id": "squad-1", "key": "@fleet?os=linux&labels=gpu"},
            })
        );
    }

    #[test]
    fn a_refusal_keeps_the_providers_reason_and_remediation() {
        let (svc, server) = service(
            "refuse",
            409,
            json!({
                "ok": false,
                "protocol_version": 2,
                "error": "no machine has a gpu",
                "reason_code": "no_matching_machine",
                "remediation": "drop the gpu label or add a gpu machine to the fleet",
            }),
        );

        let err = allocate(&svc, &linux_gpu(), Scope::Squad("squad-1"), "k").expect_err("refused");

        assert_eq!(err.reason_code, "no_matching_machine");
        let shown = err.to_string();
        assert!(shown.contains("no machine has a gpu"), "{shown}");
        assert!(shown.contains("drop the gpu label"), "{shown}");
        server.join().unwrap();
    }

    #[test]
    fn a_forgotten_allocation_is_unknown() {
        let (svc, server) = service("gone", 404, json!({}));

        let err = get(&svc, "alloc-1").expect_err("404");

        assert_eq!(err.reason_code, REASON_UNKNOWN_ALLOCATION);
        let (method, url, ..) = server.join().unwrap();
        assert_eq!(
            (method.as_str(), url.as_str()),
            ("GET", "/v1/allocations/alloc-1")
        );
    }

    #[test]
    fn a_reply_in_another_contract_version_is_refused() {
        let (svc, server) = service("version", 200, json!({"protocol_version": 1}));

        let err = set_idle(&svc, "alloc-1", true).expect_err("v1");

        assert_eq!(err.reason_code, "unsupported");
        let (method, url, _, body) = server.join().unwrap();
        assert_eq!(
            (method.as_str(), url.as_str()),
            ("POST", "/v1/allocations/alloc-1/idle")
        );
        assert_eq!(body, json!({"idle": true}));
    }

    #[test]
    fn an_unset_token_makes_the_provider_unavailable_without_calling_it() {
        let svc = ProviderService {
            name: "fleet".to_string(),
            url: "http://127.0.0.1:9".to_string(),
            token_env: "RALPHUS_TEST_ALLOC_NEVER_SET_7Q".to_string(),
        };

        let err = release(&svc, "alloc-1").expect_err("no token");

        assert_eq!(err.reason_code, REASON_UNAVAILABLE);
        assert!(
            err.to_string().contains("RALPHUS_TEST_ALLOC_NEVER_SET_7Q"),
            "{err}"
        );
    }

    /// A provider service that serves both its allocation API and every
    /// allocation's Machine API from one listener, answering by `route`, and
    /// records `"METHOD /path"` for each request. Stops when dropped.
    struct FakeFleet {
        service: ProviderService,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeFleet {
        fn start(
            name: &str,
            route: impl Fn(&str, &str, &Value) -> (u16, Value) + Send + 'static,
        ) -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}", server.server_addr());
            let token_env = format!(
                "RALPHUS_TEST_FLEET_{}",
                name.to_uppercase().replace('-', "_")
            );
            crate::provider_services::set_token_for_test(&token_env, "svc-token");
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (seen, stopping, base) = (
                std::sync::Arc::clone(&requests),
                std::sync::Arc::clone(&stop),
                url.clone(),
            );
            std::thread::spawn(move || {
                while !stopping.load(std::sync::atomic::Ordering::SeqCst) {
                    let Ok(Some(mut req)) = server.recv_timeout(Duration::from_millis(50)) else {
                        continue;
                    };
                    let mut body = String::new();
                    req.as_reader().read_to_string(&mut body).unwrap();
                    let body = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let method = req.method().to_string();
                    let path = req.url().to_string();
                    seen.lock().unwrap().push(format!("{method} {path}"));
                    let (status, mut reply) = route(&method, &path, &body);
                    // Endpoints the stub hands out point back at itself.
                    if let Some(endpoint) = reply.get("endpoint").and_then(Value::as_str) {
                        let endpoint = format!("{base}{endpoint}");
                        reply["endpoint"] = endpoint.into();
                    }
                    let _ = req.respond(
                        tiny_http::Response::from_string(reply.to_string())
                            .with_status_code(status),
                    );
                }
            });
            Self {
                service: ProviderService {
                    name: name.to_string(),
                    url,
                    token_env,
                },
                requests,
                stop,
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for FakeFleet {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn granted(id: &str) -> Value {
        json!({
            "protocol_version": 2,
            "allocation_id": id,
            "endpoint": format!("/v1/m/{id}"),
            "token": format!("token-{id}"),
            "os": "linux",
            "description": "a test machine",
        })
    }

    fn refused(reason: &str) -> (u16, Value) {
        (
            409,
            json!({
                "ok": false,
                "protocol_version": 2,
                "error": reason,
                "reason_code": "no_matching_machine",
                "remediation": "add such a machine",
            }),
        )
    }

    fn handle(store: Store) -> crate::store_lock::StoreHandle {
        std::sync::Arc::new(crate::store_lock::StoreMutex::new(store))
    }

    #[test]
    fn a_provider_list_is_walked_until_one_grants() {
        let refusing = FakeFleet::start("walk-a", |_, _, _| refused("no linux here"));
        let granting = FakeFleet::start("walk-b", |_, _, _| (200, granted("alloc-b")));
        crate::provider_services::set_services_for_test(vec![
            refusing.service.clone(),
            granting.service.clone(),
        ]);
        let store = handle(Store::open_in_memory().unwrap());

        let machine = grant(&store, "@walk-a,walk-b?os=linux", Scope::Squad("squad-1"))
            .expect("the second provider grants");

        let (request, row) = parse_bound(&machine).expect("bound");
        assert_eq!(request, "@walk-a,walk-b?os=linux");
        let row = store.lock().machine_allocation(row).unwrap().unwrap();
        assert_eq!(row.provider, "walk-b");
        assert_eq!(row.allocation_id, "alloc-b");
        assert_eq!(row.squad_id.as_deref(), Some("squad-1"));
        assert_eq!(refusing.requests(), ["POST /v1/allocations"]);
    }

    #[test]
    fn when_no_provider_grants_every_refusal_is_reported() {
        let only = FakeFleet::start("none-a", |_, _, _| refused("all busy"));
        crate::provider_services::set_services_for_test(vec![only.service.clone()]);
        let store = handle(Store::open_in_memory().unwrap());

        let err = grant(&store, "@none-a,local?labels=gpu", Scope::Squad("s"))
            .expect_err("nothing grants");

        assert!(err.contains("all busy"), "{err}");
        assert!(err.contains("add such a machine"), "{err}");
        assert!(
            err.contains("local: the daemon host carries no labels"),
            "{err}"
        );
        assert!(store.lock().machine_allocations(true).unwrap().is_empty());
    }

    #[test]
    fn local_grants_a_request_the_daemon_host_satisfies() {
        crate::provider_services::set_services_for_test(Vec::new());
        let store = handle(Store::open_in_memory().unwrap());
        let host = format!("@local?os={}", std::env::consts::OS);

        assert_eq!(grant(&store, &host, Scope::Squad("s")).unwrap(), "local");
        let other = if std::env::consts::OS == "linux" {
            "windows"
        } else {
            "linux"
        };
        let err =
            grant(&store, &format!("@local?os={other}"), Scope::Squad("s")).expect_err("wrong os");
        assert!(err.contains(&format!("not {other}")), "{err}");
    }

    #[test]
    fn a_squads_equal_requests_share_one_allocation_that_its_cells_then_run_on() {
        let fleet = FakeFleet::start("bind", |method, path, _| match (method, path) {
            ("POST", "/v1/allocations") => (200, granted("alloc-1")),
            ("POST", "/v1/m/alloc-1/ping") => (
                200,
                json!({"ok": true, "protocol_version": 2, "detail": "pong"}),
            ),
            ("POST", "/v1/allocations/alloc-1/renew") => (200, granted("alloc-1")),
            _ => (404, json!({})),
        });
        crate::provider_services::set_services_for_test(vec![fleet.service.clone()]);
        let mut store = Store::open_in_memory().unwrap();
        let file: ralphus_core::schema::TaskFile = toml::from_str(
            "[[task]]\nname=\"a\"\nmachine=\"bind\"\n\
             [[task.cell]]\ncommand=\"echo 1\"\nmode=\"raw\"\n\
             [[task.cell]]\ncommand=\"echo 2\"\nmode=\"raw\"\n",
        )
        .unwrap();
        let squad_id = store.insert_squad(&file, None, false).unwrap();
        let store = handle(store);
        let mut cells = store.lock().cells_of(&squad_id).unwrap();

        bind_squad(&store, &squad_id, &mut cells).expect("granted");

        assert_eq!(fleet.requests(), ["POST /v1/allocations"]);
        let stored = store.lock().cells_of(&squad_id).unwrap();
        let machines: Vec<_> = stored.iter().map(|c| c.machine.clone().unwrap()).collect();
        assert_eq!(machines[0], machines[1], "both cells share the allocation");
        assert_eq!(cells[0].machine, stored[0].machine);
        let (request, _) = parse_bound(&machines[0]).expect("bound");
        assert_eq!(request, "@bind");

        // The bound value runs on the allocation's Machine API with its token,
        // and a restart (no token in memory) renews one first.
        let spec = crate::runner::RunnerSpec::for_command_proof(
            &squad_id,
            "c",
            &machines[0],
            ".",
            "",
            "claude",
            None,
        );
        let runner = crate::remote_runner::provider_from_store(&store.lock(), &machines[0])
            .unwrap()
            .expect("remote");
        assert_eq!(runner.ping(&spec).unwrap().as_deref(), Some("pong"));
        TOKENS.lock().unwrap().clear();
        assert_eq!(runner.ping(&spec).unwrap().as_deref(), Some("pong"));
        assert_eq!(
            fleet.requests(),
            [
                "POST /v1/allocations",
                "POST /v1/m/alloc-1/ping",
                "POST /v1/allocations/alloc-1/renew",
                "POST /v1/m/alloc-1/ping",
            ]
        );
    }

    #[test]
    fn an_allocation_nothing_needs_is_released_and_a_forgotten_one_is_lost() {
        let fleet = FakeFleet::start("sweep", |method, path, _| match (method, path) {
            ("GET", "/v1/allocations/kept") | ("GET", "/v1/allocations/done") => {
                (200, granted("x"))
            }
            ("GET", "/v1/allocations/gone") => (404, json!({})),
            ("DELETE", "/v1/allocations/done") => (200, json!({"protocol_version": 2})),
            _ => (404, json!({})),
        });
        crate::provider_services::set_services_for_test(vec![fleet.service.clone()]);
        let mut store = Store::open_in_memory().unwrap();
        let file: ralphus_core::schema::TaskFile =
            toml::from_str("[[task]]\nname=\"a\"\n[[task.cell]]\ncommand=\"echo\"\nmode=\"raw\"\n")
                .unwrap();
        let busy = store.insert_squad(&file, None, false).unwrap();
        let finished = store.insert_squad(&file, None, false).unwrap();
        store
            .set_squad_state(&finished, crate::store::SquadState::Done)
            .unwrap();
        let alloc = |id: &str| Allocation {
            allocation_id: id.to_string(),
            endpoint: "http://unused".to_string(),
            token: "t".to_string(),
            token_expires_at_ms: None,
            os: None,
            arch: None,
            labels: Vec::new(),
            description: None,
        };
        let kept = store
            .insert_machine_allocation("sweep", &alloc("kept"), "@sweep", Scope::Squad(&busy))
            .unwrap();
        let done = store
            .insert_machine_allocation("sweep", &alloc("done"), "@sweep", Scope::Squad(&finished))
            .unwrap();
        let gone = store
            .insert_machine_allocation("sweep", &alloc("gone"), "@sweep", Scope::Squad(&busy))
            .unwrap();
        let store = handle(store);
        REATTACHED.store(false, std::sync::atomic::Ordering::SeqCst);

        maintain(&store);

        let state = |id| store.lock().machine_allocation(id).unwrap().unwrap().state;
        assert_eq!(state(kept), STATE_ACTIVE);
        assert_eq!(state(done), STATE_RELEASED);
        assert_eq!(state(gone), STATE_LOST);
        assert!(
            fleet
                .requests()
                .contains(&"DELETE /v1/allocations/done".to_string())
        );
        let lost_mail: i64 = store
            .lock()
            .conn
            .query_row(
                "SELECT COUNT(*) FROM mailbox_messages WHERE category='machine_allocation'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(lost_mail, 1, "a lost machine is reported with remediation");
    }
}
