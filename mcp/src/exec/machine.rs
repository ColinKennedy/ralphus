//! Mirrors `ralphus_cli::commands::machine::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::machine::MachineCommand;

use super::{ExecResult, usage};

pub fn execute(cmd: MachineCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        MachineCommand::Help | MachineCommand::UsageError(_) => Err(usage("no such tool")),
        MachineCommand::Register {
            scheme,
            program,
            description,
            args,
            channel,
        } => Ok(client.register_machine(
            &scheme,
            &program,
            &description,
            Some(&args),
            None,
            channel,
        )?),
        MachineCommand::List => Ok(client.list_machines()?),
        MachineCommand::Get { scheme } => Ok(client.get_machine(&scheme)?),
        MachineCommand::Remove { scheme } => Ok(client.deregister_machine(&scheme)?),
        MachineCommand::Cleanup { machine } => Ok(client.cleanup_machine(&machine)?),
    }
}
