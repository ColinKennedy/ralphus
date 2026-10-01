//! Mirrors `ralphus_cli::commands::waypoint::dispatch`.

use ralphus_cli::client::DaemonClient;
use ralphus_cli::commands::waypoint::{
    self, WaypointAffectedCommand, WaypointBearingCommand, WaypointCommand,
};
use serde_json::json;

use super::{ExecResult, usage};

pub fn execute(cmd: WaypointCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        WaypointCommand::Help | WaypointCommand::UsageError(_) => Err(usage("no such tool")),
        WaypointCommand::Create {
            prompt,
            label,
            agent,
            model,
            allow_advisory,
            affected,
        } => {
            let mut entries = Vec::with_capacity(affected.len());
            for spec in &affected {
                entries.push(waypoint::parse_affected_spec(spec)?);
            }
            Ok(client.waypoint_create(
                &prompt,
                label.as_deref(),
                agent.as_deref(),
                model.as_deref(),
                allow_advisory,
                &entries,
            )?)
        }
        WaypointCommand::List { project, state } => {
            Ok(client.waypoint_list(project.as_deref(), state.as_deref())?)
        }
        WaypointCommand::Get { waypoint_id } => Ok(client.waypoint_get(&waypoint_id)?),
        WaypointCommand::Close { waypoint_id } => Ok(client.waypoint_close(&waypoint_id)?),
        WaypointCommand::Reopen { waypoint_id } => Ok(client.waypoint_reopen(&waypoint_id)?),
        WaypointCommand::Edit {
            waypoint_id,
            label,
            prompt,
            agent,
            model,
            allow_advisory,
            resurvey,
        } => {
            let current = client.waypoint_get(&waypoint_id)?;
            let field = |name: &str| -> Option<String> {
                current
                    .get(name)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            };
            let merged_prompt = prompt.or_else(|| field("prompt")).unwrap_or_default();
            let merged_allow = allow_advisory.unwrap_or_else(|| {
                current
                    .get("allow_advisory")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            });
            Ok(client.waypoint_update(
                &waypoint_id,
                label.or_else(|| field("label")).as_deref(),
                &merged_prompt,
                agent.or_else(|| field("agent")).as_deref(),
                model.or_else(|| field("model")).as_deref(),
                merged_allow,
                resurvey,
            )?)
        }
        WaypointCommand::RosterAdd {
            waypoint_id,
            entry_id,
            note,
        } => {
            let kind = if entry_id.starts_with("guardian-") {
                "review"
            } else {
                "squad"
            };
            Ok(client.waypoint_add_roster_entry(&waypoint_id, kind, &entry_id, note.as_deref())?)
        }
        WaypointCommand::RosterRemove {
            waypoint_id,
            entry_id,
        } => Ok(client.waypoint_remove_roster_entry(&waypoint_id, &entry_id)?),
        WaypointCommand::ResurveyPreview { waypoint_id } => {
            Ok(client.waypoint_resurvey_preview(&waypoint_id)?)
        }
        WaypointCommand::Redo {
            waypoint_id,
            entry_id,
        } => Ok(client.waypoint_redo_affected_entry(&waypoint_id, &entry_id)?),
        WaypointCommand::Affected(c) => exec_affected(c, client),
        WaypointCommand::Bearing(c) => exec_bearing(c, client),
        WaypointCommand::Bearings { waypoint_id } => {
            Ok(client.waypoint_list_bearings(&waypoint_id)?)
        }
        WaypointCommand::Deliveries { waypoint_id } => {
            Ok(client.waypoint_deliveries(&waypoint_id)?)
        }
    }
    .map(|v| if v.is_null() { json!({}) } else { v })
}

fn exec_affected(cmd: WaypointAffectedCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        WaypointAffectedCommand::Help | WaypointAffectedCommand::UsageError(_) => {
            Err(usage("no such tool"))
        }
        WaypointAffectedCommand::Add {
            waypoint_id,
            kind,
            entry_id,
            mode,
        } => Ok(client.waypoint_add_affected_entry(
            &waypoint_id,
            &kind,
            &entry_id,
            mode.as_deref(),
        )?),
        WaypointAffectedCommand::Remove {
            waypoint_id,
            entry_id,
        } => Ok(client.waypoint_remove_affected_entry(&waypoint_id, &entry_id)?),
        WaypointAffectedCommand::Mode {
            waypoint_id,
            entry_id,
            mode,
        } => Ok(client.waypoint_patch_affected_entry(&waypoint_id, &entry_id, &mode)?),
    }
}

fn exec_bearing(cmd: WaypointBearingCommand, client: &DaemonClient) -> ExecResult {
    match cmd {
        WaypointBearingCommand::Help | WaypointBearingCommand::UsageError(_) => {
            Err(usage("no such tool"))
        }
        WaypointBearingCommand::Add {
            waypoint_id,
            producer_kind,
            producer_id,
            summary,
            entity_uri,
            commit_id,
            commit_summary,
        } => Ok(client.waypoint_append_bearing(
            &waypoint_id,
            &producer_kind,
            &producer_id,
            &summary,
            entity_uri.as_deref(),
            commit_id.as_deref(),
            commit_summary.as_deref(),
        )?),
    }
}
