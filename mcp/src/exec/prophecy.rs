//! Mirrors `ralphus_cli::commands::prophecy::dispatch`.

use ralphus_cli::client::{DaemonClient, ProphecyFilters};
use ralphus_cli::commands::prophecy::ProphecyCommand;

use super::{ExecResult, usage};

pub fn execute(cmd: ProphecyCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        ProphecyCommand::UsageError(_) => Err(usage("no such tool")),
        ProphecyCommand::List {
            entity,
            kind,
            q,
            limit,
            offset,
            ascending,
        } => {
            let filters = ProphecyFilters {
                entity: entity.as_deref(),
                kind: kind.as_deref(),
                q: q.as_deref(),
                limit,
                offset,
                ascending,
            };
            Ok(client.prophecies(filters)?)
        }
        ProphecyCommand::Show { id } => Ok(client.prophecy_get(id)?),
    }
}
