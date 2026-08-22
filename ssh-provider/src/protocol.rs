//! The provider JSON envelope (`docs/machine-providers.md`), modeled on
//! `examples/providers/loopback.py`'s wire format rather than derived fresh
//! from doc prose (RAL-200).
//!
//! Every reply carries `protocol_version`; on success it carries whatever
//! verb-specific fields apply (`result` for `exec`, `detail` for `ping`); on
//! failure it carries `ok: false` and an `error` string. Exactly one JSON
//! object goes to stdout per invocation -- everything else this program wants
//! to say goes to stderr (diagnostics, and `RALPHUS_EVENT:`-prefixed lines
//! forwarded from the remote cell).

use serde_json::{Value, json};

/// The provider-contract version this executable implements. Must match
/// `crate::machines::PROTOCOL_VERSION` in the daemon -- a mismatch is refused
/// at resolution time rather than invoked and hoped for.
pub const PROTOCOL_VERSION: i64 = 1;

/// Print the single JSON envelope this invocation is allowed to produce on
/// stdout. `extra` fields are merged on top of `{"protocol_version": N}`.
pub fn reply(extra: Value) {
    let mut payload = json!({ "protocol_version": PROTOCOL_VERSION });
    if let Value::Object(extra_obj) = extra {
        if let Some(obj) = payload.as_object_mut() {
            obj.extend(extra_obj);
        }
    }
    // The entire point of this process's stdout is this one line -- see the
    // module docs and the crate-level `#![allow(clippy::print_stdout)]`.
    println!("{payload}");
}

/// Reply that the *invocation* failed (an infrastructure problem: couldn't
/// reach the host, couldn't sync source, couldn't spawn ssh). Distinct from a
/// cell that ran and failed, which is `ok: true` with a `result.status` of
/// `"failed"` -- collapsing the two would make a broken connection
/// indistinguishable from legitimately failing work.
pub fn reply_err(message: impl AsRef<str>) {
    reply(json!({ "ok": false, "error": message.as_ref() }));
}

/// Reply that `exec` ran (synchronously) and produced `result` -- the remote
/// `ralphus-runner`'s own `CellResult` JSON, forwarded through unchanged
/// rather than re-typed, so this provider never drifts out of sync with the
/// daemon's `RunnerResult` shape.
pub fn reply_exec_result(result: Value) {
    reply(json!({ "ok": true, "result": result }));
}

/// Reply to `ping`: reachable, with an optional human-readable detail.
pub fn reply_ping_ok(detail: Option<String>) {
    reply(json!({ "ok": true, "detail": detail }));
}

#[cfg(test)]
mod tests {
    use super::*;

    // `reply`/`reply_err`/`reply_exec_result`/`reply_ping_ok` print straight
    // to stdout, so they're exercised end-to-end by the binary-level tests in
    // `tests/exec.rs` (which run the built executable and parse its real
    // stdout) rather than unit-tested here.

    #[test]
    fn protocol_version_matches_the_daemon_contract() {
        // `daemon/src/machines.rs::PROTOCOL_VERSION` is the daemon's side of
        // this pin -- both must move together, which is why each is a
        // documented constant rather than a magic number at each call site.
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
