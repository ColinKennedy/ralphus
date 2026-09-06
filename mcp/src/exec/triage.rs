//! Mirrors `ralphus_cli::commands::triage::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::triage::{TriageCommand, TriagePoolCommand, TriageTypeCommand};

use super::{ExecResult, usage};

pub fn execute(cmd: TriageCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        TriageCommand::Help | TriageCommand::UsageError(_) => Err(usage("no such tool")),
        TriageCommand::Pool(c) => execute_pool(c, client),
        TriageCommand::Type(c) => execute_type(c, client),
    }
}

fn execute_pool(cmd: TriagePoolCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        TriagePoolCommand::Help | TriagePoolCommand::UsageError(_) => Err(usage("no such tool")),
        TriagePoolCommand::List => Ok(client.list_triage_pools()?),
        TriagePoolCommand::Threshold {
            project,
            triage_type,
            threshold,
        } => Ok(client.set_triage_pool_threshold(&project, &triage_type, threshold)?),
    }
}

fn execute_type(cmd: TriageTypeCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        TriageTypeCommand::Help | TriageTypeCommand::UsageError(_) => Err(usage("no such tool")),
        TriageTypeCommand::Register {
            name,
            label,
            description,
        } => Ok(client.register_triage_type(&name, &label, &description)?),
        TriageTypeCommand::List => Ok(client.list_triage_types()?),
        TriageTypeCommand::Get { name } => Ok(client.get_triage_type(&name)?),
        TriageTypeCommand::Deregister { name } => Ok(client.deregister_triage_type(&name)?),
    }
}
