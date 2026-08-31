//! Mirrors `ralphus_cli::commands::show::dispatch`. Textual, no daemon call.

use ralphus_cli::commands::show::ShowCommand;
use ralphus_cli::help_map;
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: ShowCommand) -> ExecResult {
    match cmd {
        ShowCommand::Help | ShowCommand::UsageError(_) => Err(usage("no such tool")),
        ShowCommand::HelpMap => Ok(json!({"help_map": help_map::full_output()})),
    }
}
