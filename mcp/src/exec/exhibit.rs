//! Mirrors `ralphus_cli::commands::exhibit::dispatch`: captures on the
//! machine this MCP server runs on (an agent's own machine), no daemon call.

use ralphus_cli::commands::exhibit::{ExhibitCommand, run};
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: ExhibitCommand) -> ExecResult {
    if matches!(cmd, ExhibitCommand::Help) {
        return Err(usage("no such tool"));
    }
    let dir = std::env::var_os("RALPHUS_EXHIBIT_DIR");
    let path = run(&cmd, dir.as_deref()).map_err(usage)?;
    Ok(json!({ "path": path.to_string_lossy() }))
}
