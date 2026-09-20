//! The RAL-301 parity check: a real `cargo test` (part of the normal
//! `cargo test -p ralphus-mcp` / workspace `cargo test --all-targets` run
//! `AGENTS.md` already asks developers to run, and the same invocation
//! `.github/workflows/ci.yml`'s `rust` job uses -- no separate CI-only
//! script or gate exists for this).
//!
//! Bidirectional, against the single source of truth
//! (`help_map::registered_leaves()`, the same tree
//! `cli/tests/help_map_command_parity.rs` already proved is real and
//! dispatchable):
//! - every non-excluded CLI leaf has a corresponding MCP tool
//! - every MCP tool corresponds to a real, dispatchable CLI leaf
//! - every exclusion names a real leaf and carries a non-empty, substantive
//!   reason (an exclusion is a deliberate judgment call, not an escape
//!   hatch for "not implemented yet" -- see `exclusions.rs`'s doc comment)

use std::collections::BTreeSet;

use ralphus_cli::commands;
use ralphus_cli::help_map;
use ralphus_mcp::exclusions::EXCLUDED;
use ralphus_mcp::tools::all_tools;

#[test]
fn every_registered_leaf_is_covered_by_a_tool_or_a_documented_exclusion() {
    let leaves: BTreeSet<Vec<&str>> = help_map::registered_leaves()
        .into_iter()
        .map(|(path, _)| path)
        .filter(|path| !path.is_empty())
        .collect();
    let tool_paths: BTreeSet<Vec<&str>> = all_tools().into_iter().map(|t| t.path).collect();
    let excluded_paths: BTreeSet<Vec<&str>> = EXCLUDED.iter().map(|(p, _)| p.to_vec()).collect();

    let covered: BTreeSet<Vec<&str>> = tool_paths.union(&excluded_paths).cloned().collect();
    let uncovered: Vec<&Vec<&str>> = leaves.difference(&covered).collect();
    assert!(
        uncovered.is_empty(),
        "these CLI leaves have neither an MCP tool nor a documented exclusion: {uncovered:?}"
    );

    let orphaned_exclusions: Vec<&Vec<&str>> = excluded_paths.difference(&leaves).collect();
    assert!(
        orphaned_exclusions.is_empty(),
        "these exclusions name a leaf that no longer exists in help_map.rs (stale or typo'd): \
         {orphaned_exclusions:?}"
    );

    let overlap: Vec<&Vec<&str>> = tool_paths.intersection(&excluded_paths).collect();
    assert!(
        overlap.is_empty(),
        "these leaves are both tooled AND excluded -- pick one: {overlap:?}"
    );
}

#[test]
fn every_exclusion_has_a_non_empty_substantive_reason() {
    let mut bad = Vec::new();
    for (path, reason) in EXCLUDED {
        if reason.trim().len() < 15 {
            bad.push(format!("{path:?}: {reason:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "every exclusion needs a real, documented reason (not empty/placeholder text):\n{}",
        bad.join("\n")
    );
}

#[test]
fn no_excluded_leaf_secretly_has_a_tool() {
    // Belt-and-suspenders on top of the set-overlap check above: excluded
    // paths must not appear in `tools/list` at all.
    let names: BTreeSet<Vec<&str>> = all_tools().into_iter().map(|t| t.path).collect();
    for (path, _) in EXCLUDED {
        assert!(
            !names.contains(*path),
            "{path:?} is excluded but still produced a tool"
        );
    }
}

/// Picks a dummy JSON value for one chip, mirroring
/// `cli/tests/help_map_command_parity.rs::dummy_value_for` -- kept as its
/// own small copy rather than a shared crate, since the two tests serialize
/// to different shapes (raw argv strings there, JSON `Value`s here) even
/// though the underlying "what's a safe placeholder" logic is the same.
fn dummy_json(chip: &ralphus_mcp::chip::Chip) -> serde_json::Value {
    if !chip.takes_value {
        return serde_json::Value::Bool(true);
    }
    // `--input [name=value...]` (review checks/action run) parses each
    // value as `KEY=VALUE` and errors on anything else -- `Chip` doesn't
    // retain the original `name=value` hint text, so this is matched by
    // property name instead, the same way `chip.rs`'s own module doc singles
    // this shape out as a special case.
    let scalar = if chip.property_name() == "input" {
        serde_json::Value::String("x=1".to_string())
    } else {
        chip.choices.as_ref().map_or_else(
            || serde_json::Value::String("1".to_string()),
            |choices| serde_json::Value::String(choices.first().cloned().unwrap_or_default()),
        )
    };
    if chip.repeatable {
        serde_json::Value::Array(vec![scalar])
    } else {
        scalar
    }
}

/// Same exception as `cli/tests/help_map_command_parity.rs`'s
/// `BUSINESS_RULE_ONLY_EXTRA_ARGS` and for the identical reason: `review
/// squash` needs exactly one of two bare boolean flags, a business rule with
/// no chip-level "required" signal to derive from.
const BUSINESS_RULE_ONLY_EXTRA_BOOL: &[(&[&str], &str)] = &[
    (&["review", "squash"], "on"),
    (&["mailbox", "set-preferences"], "auto_watch"),
];

#[test]
fn every_tool_is_dispatchable_by_commands_parse_args() {
    let mut failures = Vec::new();
    for tool in all_tools() {
        // Fills every positional and every value-taking option (all of
        // which are required-in-practice or harmless-if-extra -- see
        // `cli/tests/help_map_command_parity.rs`'s twin heuristic and its
        // doc comment for why option chips can't distinguish "required"
        // from "optional"), but leaves bare boolean flags unset by default:
        // a handful of commands (`review squash`'s `--on`/`--off`) reject
        // having more than one boolean flag set at once, so "set none of
        // them" is the one default that's never wrong -- except when a
        // command instead requires exactly one, which
        // `BUSINESS_RULE_ONLY_EXTRA_BOOL` covers explicitly.
        let mut args = serde_json::Map::new();
        for chip in tool.positionals.iter().chain(tool.options.iter()) {
            if chip.takes_value {
                args.insert(chip.property_name(), dummy_json(chip));
            }
        }
        if let Some((_, prop)) = BUSINESS_RULE_ONLY_EXTRA_BOOL
            .iter()
            .find(|(p, _)| **p == tool.path)
        {
            args.insert((*prop).to_string(), serde_json::Value::Bool(true));
        }
        let argv = match tool.build_argv(&args) {
            Ok(argv) => argv,
            Err(e) => {
                failures.push(format!("{}: build_argv failed: {e}", tool.name));
                continue;
            }
        };
        let cmd = commands::parse_args(&argv);
        let debug = format!("{cmd:?}");
        if debug.contains("UsageError") {
            failures.push(format!("{} (argv {argv:?}) -> {debug}", tool.name));
        }
    }
    assert!(
        failures.is_empty(),
        "MCP tools that commands::parse_args does not recognize as real commands:\n{}",
        failures.join("\n")
    );
}
