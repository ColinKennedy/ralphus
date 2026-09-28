//! Mirrors `ralphus_cli::commands::prophecy::dispatch` -- both leaves are
//! plain `DaemonClient` reads, so this is a direct pass-through of the
//! already-fetched `Value` rather than any human-text rendering.

use ralphus_cli::client::{DaemonClient, ProphecyFilters};
use ralphus_cli::commands::prophecy::ProphecyCommand;

use super::{ExecResult, usage};

pub fn execute(cmd: ProphecyCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ProphecyCommand::List {
            entity_uri,
            squad_id,
            guardian_id,
            limit,
            offset,
        } => Ok(client.list_prophecies(ProphecyFilters {
            entity_uri: entity_uri.as_deref(),
            squad_id: squad_id.as_deref(),
            guardian_id: guardian_id.as_deref(),
            limit,
            offset,
        })?),
        ProphecyCommand::Show { entity_uri } => Ok(client.show_prophecy(&entity_uri)?),
        ProphecyCommand::Help | ProphecyCommand::UsageError(_) => Err(usage("no such tool")),
    }
}
