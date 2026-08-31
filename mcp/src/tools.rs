//! Builds the MCP tool registry directly from `help_map::registered_leaves()`
//! (RAL-301) -- the same tree `cli-rs/tests/help_map_command_parity.rs`
//! already made trustworthy against `commands::parse_args`, reused here per
//! the ticket's own suggestion rather than re-declaring the command surface
//! a third time.

use ralphus_cli::help_map;
use serde_json::{Map, Value, json};

use crate::chip::{Chip, parse_chip};
use crate::exclusions::is_excluded;

/// One MCP-callable command: `help_map.rs`'s leaf metadata, reshaped for
/// `tools/list`/`tools/call`.
#[derive(Debug, Clone)]
pub struct Tool {
    /// `path.join("_")`, e.g. `task_set_status` -- MCP tool names can't
    /// contain spaces, so the CLI's space-separated path is joined with `_`
    /// (a leaf name may itself already contain `-`, which is left as-is).
    pub name: String,
    pub path: Vec<&'static str>,
    pub description: String,
    pub read_only: bool,
    pub input_schema: Value,
    /// Parsed positional/option chips, in `help_map.rs` declaration order --
    /// kept alongside `input_schema` (rather than re-parsed from it) so
    /// [`Tool::build_argv`] shares exactly the same [`Chip`] facts the schema
    /// was generated from.
    pub positionals: Vec<Chip>,
    pub options: Vec<Chip>,
}

impl Tool {
    /// Converts MCP tool-call `arguments` (already validated against
    /// [`Tool::input_schema`] by the caller, but re-checked here for
    /// required-ness since MCP clients aren't guaranteed to enforce
    /// `required`) into the argv `commands::parse_args` expects: the tool's
    /// path, then one token per positional (in declared order), then
    /// `--flag value`/`--flag` per option present in `arguments`.
    pub fn build_argv(&self, arguments: &Map<String, Value>) -> Result<Vec<String>, String> {
        let mut argv: Vec<String> = self.path.iter().map(|s| (*s).to_string()).collect();
        for chip in &self.positionals {
            let prop = chip.property_name();
            match arguments.get(&prop) {
                Some(value) => push_chip_values(&mut argv, chip, value, None)?,
                None if chip.required => {
                    return Err(format!("missing required argument '{prop}'"));
                }
                None => {}
            }
        }
        for chip in &self.options {
            let prop = chip.property_name();
            let Some(value) = arguments.get(&prop) else {
                continue;
            };
            if !chip.takes_value {
                if value.as_bool() == Some(true) {
                    argv.push(chip.name.clone());
                }
                continue;
            }
            push_chip_values(&mut argv, chip, value, Some(&chip.name))?;
        }
        Ok(argv)
    }
}

/// Appends one chip's argv contribution: for a repeatable chip, `value` must
/// be a JSON array and each element is pushed as its own token (preceded by
/// `flag` again for a repeatable option, matching `Scanner::take_repeated`'s
/// "every occurrence of `--name value`" convention); otherwise `value` is
/// pushed once, preceded by `flag` for an option.
fn push_chip_values(
    argv: &mut Vec<String>,
    chip: &Chip,
    value: &Value,
    flag: Option<&str>,
) -> Result<(), String> {
    if chip.repeatable {
        let items = value
            .as_array()
            .ok_or_else(|| format!("'{}' must be an array", chip.property_name()))?;
        for item in items {
            if let Some(flag) = flag {
                argv.push(flag.to_string());
            }
            argv.push(value_to_token(item, chip)?);
        }
        return Ok(());
    }
    if let Some(flag) = flag {
        argv.push(flag.to_string());
    }
    argv.push(value_to_token(value, chip)?);
    Ok(())
}

fn value_to_token(value: &Value, chip: &Chip) -> Result<String, String> {
    let token = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => {
            return Err(format!(
                "'{}' must be a string or number, got {other}",
                chip.property_name()
            ));
        }
    };
    if let Some(choices) = &chip.choices {
        if !choices.iter().any(|c| c == &token) {
            return Err(format!(
                "'{}' must be one of {choices:?}, got '{token}'",
                chip.property_name()
            ));
        }
    }
    Ok(token)
}

/// Every non-excluded leaf in `help_map::registered_leaves()`, as an MCP
/// [`Tool`]. Excluded leaves are dropped entirely -- their absence, plus
/// [`crate::exclusions::EXCLUDED`]'s documented reason, is the parity check's
/// answer for "why doesn't this CLI command have a tool."
#[must_use]
pub fn all_tools() -> Vec<Tool> {
    help_map::registered_leaves()
        .into_iter()
        .filter(|(path, _)| !path.is_empty() && !is_excluded(path))
        .map(|(path, node)| build_tool(&path, node))
        .collect()
}

fn build_tool(path: &[&'static str], node: &'static help_map::HelpNode) -> Tool {
    let positionals: Vec<Chip> = node.positionals.iter().map(|c| parse_chip(c)).collect();
    let options: Vec<Chip> = node.options.iter().map(|c| parse_chip(c)).collect();

    let mut properties = Map::new();
    let mut required = Vec::new();
    for chip in positionals.iter().chain(options.iter()) {
        let prop_name = chip.property_name();
        properties.insert(prop_name.clone(), chip_schema(chip));
        if chip.required && !chip.is_option() {
            required.push(Value::String(prop_name));
        }
    }

    Tool {
        name: path.join("_"),
        path: path.to_vec(),
        description: node.description.to_string(),
        read_only: node.read_only_safe,
        input_schema: json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": required,
        }),
        positionals,
        options,
    }
}

fn chip_schema(chip: &Chip) -> Value {
    if !chip.takes_value {
        return json!({"type": "boolean", "description": "boolean flag"});
    }
    let base = chip.choices.as_ref().map_or_else(
        || json!({"type": "string"}),
        |choices| json!({"type": "string", "enum": choices}),
    );
    if chip.repeatable {
        json!({"type": "array", "items": base})
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_leaves_produce_no_tool() {
        let tools = all_tools();
        assert!(!tools.iter().any(|t| t.path == ["cell", "open-agent"]));
        assert!(!tools.iter().any(|t| t.path.first() == Some(&"quick-start")));
    }

    #[test]
    fn every_tool_has_a_json_object_schema() {
        for tool in all_tools() {
            assert!(tool.input_schema["type"] == "object", "{}", tool.name);
            assert!(tool.input_schema["properties"].is_object(), "{}", tool.name);
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let tools = all_tools();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        let mut deduped = names.clone();
        deduped.dedup();
        assert_eq!(names, deduped, "duplicate MCP tool names");
    }

    #[test]
    fn task_show_tool_has_required_selector_property() {
        let tools = all_tools();
        let tool = tools
            .iter()
            .find(|t| t.path == ["task", "show"])
            .expect("task show tool");
        assert!(tool.input_schema["properties"]["selector"].is_object());
        assert!(
            tool.input_schema["required"]
                .as_array()
                .unwrap()
                .contains(&Value::String("selector".to_string()))
        );
    }

    #[test]
    fn read_only_safe_tag_is_preserved_for_a_known_read_only_leaf() {
        let tools = all_tools();
        let tool = tools
            .iter()
            .find(|t| t.path == ["task", "show"])
            .expect("task show tool");
        assert!(tool.read_only, "task show is read-only-safe in help_map.rs");
        let mutating = tools
            .iter()
            .find(|t| t.path == ["task", "set-status"])
            .expect("task set-status tool");
        assert!(!mutating.read_only);
    }

    #[test]
    fn build_argv_places_positionals_then_flags() {
        let tools = all_tools();
        let tool = tools
            .iter()
            .find(|t| t.path == ["task", "set-status"])
            .expect("task set-status tool");
        let mut args = Map::new();
        args.insert("selector".to_string(), json!("squad-1/build"));
        args.insert("state".to_string(), json!("done"));
        assert_eq!(
            tool.build_argv(&args).unwrap(),
            vec!["task", "set-status", "squad-1/build", "done"]
        );
    }

    #[test]
    fn build_argv_reports_missing_required_argument() {
        let tools = all_tools();
        let tool = tools
            .iter()
            .find(|t| t.path == ["task", "show"])
            .expect("task show tool");
        assert!(tool.build_argv(&Map::new()).is_err());
    }

    #[test]
    fn build_argv_includes_flag_with_value() {
        let tools = all_tools();
        let tool = tools
            .iter()
            .find(|t| t.path == ["cell", "restart-proof"])
            .expect("cell restart-proof tool");
        let mut args = Map::new();
        args.insert("selector".to_string(), json!("squad-1/build/0"));
        args.insert("from".to_string(), json!(2));
        assert_eq!(
            tool.build_argv(&args).unwrap(),
            vec!["cell", "restart-proof", "squad-1/build/0", "--from", "2"]
        );
    }
}
