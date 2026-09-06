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
        MailboxCommand::Personal {
            unread_only,
            priority,
            user,
        } => Ok(client.personal_mailbox_messages(
            unread_only,
            priority.as_deref(),
            user.as_deref(),
        )?),
        MailboxCommand::PersonalDrain { message_ids, user } => {
            Ok(client.personal_mailbox_drain(message_ids.as_deref(), user.as_deref())?)
        }
        MailboxCommand::Follow {
            entity_uri,
            tiers,
            user,
        } => {
            let tiers_opt = (!tiers.is_empty()).then_some(tiers.as_slice());
            Ok(client.create_follow(&entity_uri, tiers_opt, user.as_deref())?)
        }
        MailboxCommand::Unfollow { entity_uri, user } => {
            Ok(client.delete_follow(&entity_uri, user.as_deref())?)
        }
        MailboxCommand::Follows { user } => Ok(client.list_follows(user.as_deref())?),
        MailboxCommand::Preferences { user } => Ok(client.get_user_preferences(&user)?),
        MailboxCommand::SetPreferences {
            user,
            auto_follow,
            tiers,
        } => {
            let tiers_opt = (!tiers.is_empty()).then_some(tiers.as_slice());
            Ok(client.set_user_preferences(&user, auto_follow, tiers_opt)?)
        }
    }
}
