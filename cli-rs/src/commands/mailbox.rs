//! `ralphus mailbox <subcommand>` (RAL-241, poll-only scope): the escalation
//! mailbox client. `ralphus mailbox check` is the turn-boundary poll a
//! `ralphus quick-start watcher ...` session's system prompt is instructed to
//! run after every user turn -- see `crate::commands::quick_start`.

use serde_json::Value;

use crate::args::GlobalOpts;
use crate::client::DaemonClient;
use crate::commands::{CommandError, emit, run_and_report};
use crate::flags::Scanner;

#[derive(Debug, Clone)]
pub enum MailboxCommand {
    Check { priority: Option<String> },
    UsageError(String),
}

#[must_use]
pub fn parse(args: &[String]) -> MailboxCommand {
    let mut scanner = Scanner::new(&args[1.min(args.len())..]);
    match args.first().map(String::as_str) {
        None | Some("check") => {
            let priority = scanner.take_value("--priority").ok().flatten();
            MailboxCommand::Check { priority }
        }
        Some(other) => MailboxCommand::UsageError(format!("unknown mailbox subcommand: {other}")),
    }
}

#[must_use]
pub fn dispatch(cmd: MailboxCommand, opts: &GlobalOpts) -> i32 {
    let client = opts.client();
    match cmd {
        MailboxCommand::UsageError(m) => {
            println!("usage error: {m}");
            2
        }
        MailboxCommand::Check { priority } => run_and_report(opts, None, || {
            let client_id = ensure_client_id(&client)?;
            let messages = client.mailbox_messages(&client_id, true, priority.as_deref())?;
            let ids = message_ids(&messages);
            let drained = if ids.is_empty() {
                0
            } else {
                let result = client.mailbox_drain(&client_id, Some(&ids))?;
                result["drained"].as_u64().unwrap_or(0)
            };
            emit(
                opts,
                &serde_json::json!({"messages": messages, "drained": drained}),
                |_| render_messages(&messages),
            );
            Ok(())
        }),
    }
}

/// Reads the locally persisted mailbox `client_id`
/// (`ralphus_core::mailbox_client_id_path()`), registering a fresh one with
/// the daemon and persisting it if none exists yet. Shared by `ralphus
/// mailbox check` and `ralphus quick-start watcher ...` (which registers
/// automatically on startup, per the ticket's Q&A) so both ever mint at most
/// one `client_id` per machine.
pub fn ensure_client_id(client: &DaemonClient) -> Result<String, CommandError> {
    let path = ralphus_core::mailbox_client_id_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let result = client.mailbox_register()?;
    let client_id = result["client_id"]
        .as_str()
        .ok_or_else(|| CommandError::Usage("daemon did not return a client_id".to_string()))?
        .to_string();
    let _ = std::fs::write(&path, &client_id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(client_id)
}

pub fn message_ids(messages: &Value) -> Vec<String> {
    messages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect()
}

fn render_messages(messages: &Value) {
    let items = messages.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("mailbox: no unread messages");
        return;
    }
    for m in &items {
        let tag = match m["priority"].as_str().unwrap_or("normal") {
            "urgent" => "URGENT",
            "high" => "HIGH",
            _ => "info",
        };
        println!("[{tag}] {}", m["message"].as_str().unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_mailbox_behaves_like_check() {
        matches!(parse(&[]), MailboxCommand::Check { priority: None });
    }

    #[test]
    fn parses_check_with_priority_filter() {
        match parse(&v(&["check", "--priority", "urgent"])) {
            MailboxCommand::Check { priority } => {
                assert_eq!(priority.as_deref(), Some("urgent"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        matches!(parse(&v(&["bogus"])), MailboxCommand::UsageError(_));
    }

    #[test]
    fn message_ids_extracts_id_field() {
        let messages = serde_json::json!([{"id": "mailbox-1"}, {"id": "mailbox-2"}]);
        assert_eq!(message_ids(&messages), v(&["mailbox-1", "mailbox-2"]));
    }
}
