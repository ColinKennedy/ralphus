//! RAL-713: no CLI command may print a secret argument in its `Debug` form.
//!
//! `cli/src/main.rs` logs every parsed command whole (`{cmd:?}`), so a secret
//! flag stored as a plain `String` reaches stderr on every invocation. This
//! builds every leaf's argv (through the MCP tool registry, which covers the
//! whole `help_map` tree), plants a unique marker in each secret-named
//! argument, and fails when the marker shows up in the parsed command's
//! `Debug` output. Store such a value as `ralphus_cli::args::Secret` (or a
//! type whose `Debug` redacts it), or allow-list the argument below with the
//! reason it is not a secret.

use ralphus_cli::commands;
use ralphus_mcp::chip::Chip;
use ralphus_mcp::tools::all_tools;

const MARKER: &str = "planted-secret-marker";

/// Arguments whose names look secret but whose values are not, as
/// `(tool path, property, reason)`.
const NOT_SECRET: &[(&[&str], &str, &str)] = &[
    (
        &["cell", "edit"],
        "maximum_tool_output_tokens",
        "a token count, kept as text until the daemon validates it",
    ),
    (
        &["proof", "edit"],
        "maximum_tool_output_tokens",
        "a token count, kept as text until the daemon validates it",
    ),
];

/// Whether an argument's name says it may carry a secret. `set`/`header` are
/// the `KEY=VALUE` literal-value flags (agent profile env, MCP capability env
/// and headers).
fn looks_secret(property: &str) -> bool {
    matches!(property, "set" | "header")
        || ["token", "secret", "password", "api_key", "credential"]
            .iter()
            .any(|word| property.contains(word))
}

/// A placeholder that parses for `chip`, with `value` as the value part of a
/// `KEY=VALUE` argument or the whole value otherwise.
fn json_for(chip: &Chip, value: &str) -> serde_json::Value {
    if !chip.takes_value {
        return serde_json::Value::Bool(true);
    }
    let property = chip.property_name();
    let scalar = if matches!(
        property.as_str(),
        "input" | "set" | "link" | "header" | "header_link"
    ) {
        format!("x={value}")
    } else if let Some(choices) = &chip.choices {
        choices.first().cloned().unwrap_or_default()
    } else {
        value.to_string()
    };
    let scalar = serde_json::Value::String(scalar);
    if chip.repeatable {
        serde_json::Value::Array(vec![scalar])
    } else {
        scalar
    }
}

#[test]
fn no_command_prints_a_secret_argument_when_logged() {
    let mut leaks = Vec::new();
    let mut planted = 0;
    for tool in all_tools() {
        let chips: Vec<&Chip> = tool.positionals.iter().chain(&tool.options).collect();
        for secret in chips.iter().filter(|chip| {
            chip.takes_value && chip.choices.is_none() && looks_secret(&chip.property_name())
        }) {
            let property = secret.property_name();
            if NOT_SECRET
                .iter()
                .any(|(path, prop, _)| *path == tool.path && *prop == property)
            {
                continue;
            }
            let mut args = serde_json::Map::new();
            for chip in chips.iter().filter(|chip| chip.takes_value) {
                let value = if chip.property_name() == property {
                    MARKER
                } else {
                    "1"
                };
                args.insert(chip.property_name(), json_for(chip, value));
            }
            let argv = tool
                .build_argv(&args)
                .unwrap_or_else(|e| panic!("{}: build_argv failed: {e}", tool.name));
            let logged = format!("{:?}", commands::parse_args(&argv));
            if logged.contains("UsageError") {
                // A typed argument (a count such as `--maximum-tool-output-
                // tokens`) rejects the marker, so it cannot carry a secret.
                assert!(
                    logged.contains(&format!("invalid value '{MARKER}'")),
                    "{} (argv {argv:?}) did not parse: {logged}",
                    tool.name
                );
                continue;
            }
            planted += 1;
            if logged.contains(MARKER) {
                leaks.push(format!("{} --{property}: {logged}", tool.name));
            }
        }
    }
    assert!(planted > 0, "no secret-named argument was found to check");
    assert!(
        leaks.is_empty(),
        "these commands print a secret argument in their Debug form, which the CLI logs; \
         store it as ralphus_cli::args::Secret or allow-list it with a reason:\n{}",
        leaks.join("\n")
    );
}

#[test]
fn every_not_secret_entry_has_a_reason() {
    for (path, property, reason) in NOT_SECRET {
        assert!(reason.trim().len() >= 15, "{path:?} {property}: {reason:?}");
    }
}
