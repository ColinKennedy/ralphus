//! Mirrors `ralphus_cli::commands::mailbox::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::mailbox::{self, MailboxCommand};
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: MailboxCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        MailboxCommand::UsageError(_) => Err(usage("no such tool")),
        MailboxCommand::Check { priority } => {
            let client_id = mailbox::ensure_client_id(client)?;
            let messages = client.mailbox_messages(&client_id, true, priority.as_deref())?;
            let ids = mailbox::message_ids(&messages);
            let drained = if ids.is_empty() {
                0
            } else {
                let result = client.mailbox_drain(&client_id, Some(&ids))?;
                result["drained"].as_u64().unwrap_or(0)
            };
            Ok(json!({"messages": messages, "drained": drained}))
        }
    }
}
