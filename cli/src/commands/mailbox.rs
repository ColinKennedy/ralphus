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
    Check {
        priority: Option<String>,
    },
    Personal {
        unread_only: bool,
        priority: Option<String>,
        user: Option<String>,
    },
    PersonalDrain {
        message_ids: Option<Vec<String>>,
        user: Option<String>,
    },
    Follow {
        entity_uri: String,
        tiers: Vec<String>,
        user: Option<String>,
    },
    Unfollow {
        entity_uri: String,
        user: Option<String>,
    },
    Follows {
        user: Option<String>,
    },
    Preferences {
        user: String,
    },
    SetPreferences {
        user: String,
        auto_follow: bool,
        tiers: Vec<String>,
    },
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
        Some("personal") => {
            let unread_only = scanner.take_bool("--unread");
            let priority = match scanner.take_value("--priority") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            MailboxCommand::Personal {
                unread_only,
                priority,
                user,
            }
        }
        Some("personal-drain") => {
            let ids = match scanner.take_repeated("--id") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            MailboxCommand::PersonalDrain {
                message_ids: if ids.is_empty() { None } else { Some(ids) },
                user,
            }
        }
        Some("follow") => {
            let tiers = match scanner.take_repeated("--tier") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            match scanner.remaining().into_iter().next() {
                Some(entity_uri) => MailboxCommand::Follow {
                    entity_uri,
                    tiers,
                    user,
                },
                None => MailboxCommand::UsageError("follow requires <entity-uri>".to_string()),
            }
        }
        Some("unfollow") => {
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            match scanner.remaining().into_iter().next() {
                Some(entity_uri) => MailboxCommand::Unfollow { entity_uri, user },
                None => MailboxCommand::UsageError("unfollow requires <entity-uri>".to_string()),
            }
        }
        Some("follows") => {
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            MailboxCommand::Follows { user }
        }
        Some("preferences") => {
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            match user {
                Some(user) => MailboxCommand::Preferences { user },
                None => {
                    MailboxCommand::UsageError("preferences requires --user <name>".to_string())
                }
            }
        }
        Some("set-preferences") => {
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            let auto_follow_on = scanner.take_bool("--auto-follow");
            let auto_follow_off = scanner.take_bool("--no-auto-follow");
            let tiers = match scanner.take_repeated("--tier") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            let auto_follow = match (auto_follow_on, auto_follow_off) {
                (true, false) => Some(true),
                (false, true) => Some(false),
                _ => None,
            };
            match (user, auto_follow) {
                (Some(user), Some(auto_follow)) => MailboxCommand::SetPreferences {
                    user,
                    auto_follow,
                    tiers,
                },
                (None, _) => {
                    MailboxCommand::UsageError("set-preferences requires --user <name>".to_string())
                }
                (_, None) => MailboxCommand::UsageError(
                    "set-preferences requires exactly one of --auto-follow/--no-auto-follow"
                        .to_string(),
                ),
            }
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
        MailboxCommand::Personal {
            unread_only,
            priority,
            user,
        } => run_and_report(opts, None, || {
            let messages = client.personal_mailbox_messages(
                unread_only,
                priority.as_deref(),
                user.as_deref(),
            )?;
            emit(opts, &messages, |_| render_messages(&messages));
            Ok(())
        }),
        MailboxCommand::PersonalDrain { message_ids, user } => run_and_report(opts, None, || {
            let result = client.personal_mailbox_drain(message_ids.as_deref(), user.as_deref())?;
            emit(opts, &result, |v| {
                println!("drained {} message(s)", v["drained"].as_u64().unwrap_or(0));
            });
            Ok(())
        }),
        MailboxCommand::Follow {
            entity_uri,
            tiers,
            user,
        } => run_and_report(opts, None, || {
            let tiers_opt = (!tiers.is_empty()).then_some(tiers.as_slice());
            let result = client.create_follow(&entity_uri, tiers_opt, user.as_deref())?;
            emit(opts, &result, |v| {
                println!(
                    "followed {}",
                    v["entity_uri"].as_str().unwrap_or(&entity_uri)
                );
            });
            Ok(())
        }),
        MailboxCommand::Unfollow { entity_uri, user } => {
            run_and_report(opts, Some("ralphus mailbox follows"), || {
                let result = client.delete_follow(&entity_uri, user.as_deref())?;
                emit(opts, &result, |_| println!("unfollowed {entity_uri}"));
                Ok(())
            })
        }
        MailboxCommand::Follows { user } => run_and_report(opts, None, || {
            let result = client.list_follows(user.as_deref())?;
            emit(opts, &result, render_follows);
            Ok(())
        }),
        MailboxCommand::Preferences { user } => run_and_report(opts, None, || {
            let result = client.get_user_preferences(&user)?;
            emit(opts, &result, render_preferences);
            Ok(())
        }),
        MailboxCommand::SetPreferences {
            user,
            auto_follow,
            tiers,
        } => run_and_report(opts, None, || {
            let tiers_opt = (!tiers.is_empty()).then_some(tiers.as_slice());
            let result = client.set_user_preferences(&user, auto_follow, tiers_opt)?;
            emit(opts, &result, render_preferences);
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

fn render_follows(result: &Value) {
    let follows = result["follows"].as_array().cloned().unwrap_or_default();
    if follows.is_empty() {
        println!("no follows");
        return;
    }
    for f in &follows {
        let tiers = f["notify_tiers"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        println!(
            "{}  {}  [{}]",
            f["id"].as_str().unwrap_or_default(),
            f["entity_uri"].as_str().unwrap_or_default(),
            tiers
        );
    }
}

fn render_preferences(u: &Value) {
    let tiers = u["default_notify_tiers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    println!(
        "{}: auto_follow={} default_notify_tiers=[{}]",
        u["name"].as_str().unwrap_or_default(),
        u["auto_follow"].as_bool().unwrap_or(false),
        tiers
    );
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

    #[test]
    fn parses_personal_with_flags() {
        match parse(&v(&[
            "personal",
            "--unread",
            "--priority",
            "high",
            "--user",
            "colin",
        ])) {
            MailboxCommand::Personal {
                unread_only,
                priority,
                user,
            } => {
                assert!(unread_only);
                assert_eq!(priority.as_deref(), Some("high"));
                assert_eq!(user.as_deref(), Some("colin"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_personal_drain_with_ids() {
        match parse(&v(&[
            "personal-drain",
            "--id",
            "mailbox-1",
            "--id",
            "mailbox-2",
        ])) {
            MailboxCommand::PersonalDrain { message_ids, user } => {
                assert_eq!(message_ids, Some(v(&["mailbox-1", "mailbox-2"])));
                assert_eq!(user, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_follow_with_entity_uri_and_tiers() {
        match parse(&v(&[
            "follow",
            "--tier",
            "urgent",
            "--tier",
            "high",
            "--user",
            "colin",
            "squad:squad-1",
        ])) {
            MailboxCommand::Follow {
                entity_uri,
                tiers,
                user,
            } => {
                assert_eq!(entity_uri, "squad:squad-1");
                assert_eq!(tiers, v(&["urgent", "high"]));
                assert_eq!(user.as_deref(), Some("colin"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn follow_without_entity_uri_is_usage_error() {
        matches!(parse(&v(&["follow"])), MailboxCommand::UsageError(_));
    }

    #[test]
    fn parses_unfollow() {
        match parse(&v(&["unfollow", "squad:squad-1"])) {
            MailboxCommand::Unfollow { entity_uri, user } => {
                assert_eq!(entity_uri, "squad:squad-1");
                assert_eq!(user, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_follows_listing() {
        match parse(&v(&["follows", "--user", "colin"])) {
            MailboxCommand::Follows { user } => assert_eq!(user.as_deref(), Some("colin")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn preferences_requires_user() {
        matches!(parse(&v(&["preferences"])), MailboxCommand::UsageError(_));
        match parse(&v(&["preferences", "--user", "colin"])) {
            MailboxCommand::Preferences { user } => assert_eq!(user, "colin"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn set_preferences_requires_user_and_auto_follow_choice() {
        matches!(
            parse(&v(&["set-preferences", "--user", "colin"])),
            MailboxCommand::UsageError(_)
        );
        matches!(
            parse(&v(&["set-preferences", "--auto-follow"])),
            MailboxCommand::UsageError(_)
        );
        match parse(&v(&[
            "set-preferences",
            "--user",
            "colin",
            "--auto-follow",
            "--tier",
            "urgent",
        ])) {
            MailboxCommand::SetPreferences {
                user,
                auto_follow,
                tiers,
            } => {
                assert_eq!(user, "colin");
                assert!(auto_follow);
                assert_eq!(tiers, v(&["urgent"]));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&[
            "set-preferences",
            "--user",
            "colin",
            "--no-auto-follow",
        ])) {
            MailboxCommand::SetPreferences { auto_follow, .. } => assert!(!auto_follow),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
