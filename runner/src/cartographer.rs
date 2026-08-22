//! Structured event emission over the `RALPHUS_EVENT:` stderr marker, ported
//! from `cli/src/ralphus/runner/cartographer.py`. The runner has no direct
//! database access (it's a subprocess, possibly on a remote machine via a
//! machine provider) and stdout is reserved for the `CellSpec`/
//! `CellResult` JSON contract, so structured events piggyback on stderr
//! behind this marker -- `daemon/src/runner.rs` already reads the child's
//! stderr line-by-line looking for exactly this prefix (`EVENT_MARKER`) and
//! forwards matches into Cartographer. Kept as its own stderr hop rather than
//! collapsed into a direct `Store` call, matching the existing architecture.

use serde_json::json;

/// Must match `daemon/src/runner.rs::EVENT_MARKER` exactly.
const EVENT_MARKER: &str = "RALPHUS_EVENT: ";

#[derive(Debug, Clone, Copy, Default)]
pub struct EventContext<'a> {
    pub squad_id: Option<&'a str>,
    pub cell_id: Option<&'a str>,
    pub task: Option<&'a str>,
}

/// Emits one structured event line to stderr. `level` mirrors Python's
/// default of `"info"`.
pub fn emit(
    source: &str,
    message: &str,
    level: &str,
    ctx: EventContext<'_>,
    payload: serde_json::Value,
) {
    let mut body = json!({
        "source": source,
        "message": message,
        "level": level,
        "payload": payload,
    });
    if let Some(o) = body.as_object_mut() {
        if let Some(squad_id) = ctx.squad_id {
            o.insert("squad_id".to_string(), json!(squad_id));
        }
        if let Some(cell_id) = ctx.cell_id {
            o.insert("cell_id".to_string(), json!(cell_id));
        }
        if let Some(task) = ctx.task {
            o.insert("task".to_string(), json!(task));
        }
    }
    eprintln!("{EVENT_MARKER}{body}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_matches_daemon_constant() {
        assert_eq!(EVENT_MARKER, "RALPHUS_EVENT: ");
    }
}
