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
        /// RAL-375: restrict to one category (e.g. `"review"`). `None`
        /// drains everything, regardless of category -- QuickStart Watcher's
        /// default. QuickStart Reviewer's system prompt instead runs `check
        /// --category review`.
        category: Option<String>,
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
    Watch {
        entity_uri: String,
        tiers: Vec<String>,
        user: Option<String>,
    },
    Unwatch {
        entity_uri: String,
        user: Option<String>,
    },
    Watches {
        user: Option<String>,
    },
    Preferences {
        user: String,
    },
    SetPreferences {
        user: String,
        auto_watch: bool,
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
            let category = scanner.take_value("--category").ok().flatten();
            MailboxCommand::Check { priority, category }
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
        Some("watch") => {
            let tiers = match scanner.take_repeated("--tier") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            match scanner.remaining().into_iter().next() {
                Some(entity_uri) => MailboxCommand::Watch {
                    entity_uri,
                    tiers,
                    user,
                },
                None => MailboxCommand::UsageError("watch requires <entity-uri>".to_string()),
            }
        }
        Some("unwatch") => {
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            match scanner.remaining().into_iter().next() {
                Some(entity_uri) => MailboxCommand::Unwatch { entity_uri, user },
                None => MailboxCommand::UsageError("unwatch requires <entity-uri>".to_string()),
            }
        }
        Some("watches") => {
            let user = match scanner.take_value("--user") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            MailboxCommand::Watches { user }
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
            let auto_watch_on = scanner.take_bool("--auto-watch");
            let auto_watch_off = scanner.take_bool("--no-auto-watch");
            let tiers = match scanner.take_repeated("--tier") {
                Ok(v) => v,
                Err(e) => return MailboxCommand::UsageError(e.0),
            };
            let auto_watch = match (auto_watch_on, auto_watch_off) {
                (true, false) => Some(true),
                (false, true) => Some(false),
                _ => None,
            };
            match (user, auto_watch) {
                (Some(user), Some(auto_watch)) => MailboxCommand::SetPreferences {
                    user,
                    auto_watch,
                    tiers,
                },
                (None, _) => {
                    MailboxCommand::UsageError("set-preferences requires --user <name>".to_string())
                }
                (_, None) => MailboxCommand::UsageError(
                    "set-preferences requires exactly one of --auto-watch/--no-auto-watch"
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
        MailboxCommand::Check { priority, category } => run_and_report(opts, None, || {
            let client_id = ensure_client_id(&client)?;
            let messages = client.mailbox_messages_filtered(
                &client_id,
                true,
                priority.as_deref(),
                category.as_deref(),
            )?;
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
        MailboxCommand::Watch {
            entity_uri,
            tiers,
            user,
        } => run_and_report(opts, None, || {
            let tiers_opt = (!tiers.is_empty()).then_some(tiers.as_slice());
            let result = client.create_watch(&entity_uri, tiers_opt, user.as_deref())?;
            emit(opts, &result, |v| {
                println!(
                    "watching {}",
                    v["entity_uri"].as_str().unwrap_or(&entity_uri)
                );
            });
            Ok(())
        }),
        MailboxCommand::Unwatch { entity_uri, user } => {
            run_and_report(opts, Some("ralphus mailbox watches"), || {
                let result = client.delete_watch(&entity_uri, user.as_deref())?;
                emit(opts, &result, |_| println!("stopped watching {entity_uri}"));
                Ok(())
            })
        }
        MailboxCommand::Watches { user } => run_and_report(opts, None, || {
            let result = client.list_watches(user.as_deref())?;
            emit(opts, &result, render_watches);
            Ok(())
        }),
        MailboxCommand::Preferences { user } => run_and_report(opts, None, || {
            let result = client.get_user_preferences(&user)?;
            emit(opts, &result, render_preferences);
            Ok(())
        }),
        MailboxCommand::SetPreferences {
            user,
            auto_watch,
            tiers,
        } => run_and_report(opts, None, || {
            let tiers_opt = (!tiers.is_empty()).then_some(tiers.as_slice());
            let result = client.set_user_preferences(&user, auto_watch, tiers_opt)?;
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

fn render_watches(result: &Value) {
    let watches = result["watches"].as_array().cloned().unwrap_or_default();
    if watches.is_empty() {
        println!("no watches");
        return;
    }
    for f in &watches {
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
        "{}: auto_watch={} default_notify_tiers=[{}]",
        u["name"].as_str().unwrap_or_default(),
        u["auto_watch"].as_bool().unwrap_or(false),
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
        matches!(
            parse(&[]),
            MailboxCommand::Check {
                priority: None,
                category: None
            }
        );
    }

    #[test]
    fn parses_check_with_priority_filter() {
        match parse(&v(&["check", "--priority", "urgent"])) {
            MailboxCommand::Check { priority, category } => {
                assert_eq!(priority.as_deref(), Some("urgent"));
                assert_eq!(category, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_check_with_category_filter() {
        match parse(&v(&["check", "--category", "review"])) {
            MailboxCommand::Check { priority, category } => {
                assert_eq!(priority, None);
                assert_eq!(category.as_deref(), Some("review"));
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
    fn parses_watch_with_entity_uri_and_tiers() {
        match parse(&v(&[
            "watch",
            "--tier",
            "urgent",
            "--tier",
            "high",
            "--user",
            "colin",
            "squad:squad-1",
        ])) {
            MailboxCommand::Watch {
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
    fn watch_without_entity_uri_is_usage_error() {
        assert!(matches!(
            parse(&v(&["watch"])),
            MailboxCommand::UsageError(_)
        ));
    }

    #[test]
    fn parses_unwatch() {
        match parse(&v(&["unwatch", "squad:squad-1"])) {
            MailboxCommand::Unwatch { entity_uri, user } => {
                assert_eq!(entity_uri, "squad:squad-1");
                assert_eq!(user, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_watches_listing() {
        match parse(&v(&["watches", "--user", "colin"])) {
            MailboxCommand::Watches { user } => assert_eq!(user.as_deref(), Some("colin")),
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
    fn set_preferences_requires_user_and_auto_watch_choice() {
        matches!(
            parse(&v(&["set-preferences", "--user", "colin"])),
            MailboxCommand::UsageError(_)
        );
        matches!(
            parse(&v(&["set-preferences", "--auto-watch"])),
            MailboxCommand::UsageError(_)
        );
        match parse(&v(&[
            "set-preferences",
            "--user",
            "colin",
            "--auto-watch",
            "--tier",
            "urgent",
        ])) {
            MailboxCommand::SetPreferences {
                user,
                auto_watch,
                tiers,
            } => {
                assert_eq!(user, "colin");
                assert!(auto_watch);
                assert_eq!(tiers, v(&["urgent"]));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse(&v(&[
            "set-preferences",
            "--user",
            "colin",
            "--no-auto-watch",
        ])) {
            MailboxCommand::SetPreferences { auto_watch, .. } => assert!(!auto_watch),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
