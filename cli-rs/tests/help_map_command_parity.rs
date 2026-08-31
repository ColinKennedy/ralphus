//! Drift-safety net between `help_map.rs`'s hand-authored command tree and
//! the real command surface in `commands/*.rs`'s `parse()` functions
//! (RAL-301 prerequisite). Nothing in the type system stops those two lists
//! from disagreeing -- a `HelpNode` added without a matching parser arm, or
//! a parser arm added without a matching `HelpNode`, both compile cleanly.
//! This test closes that gap by actually invoking `commands::parse_args` on
//! synthetic argv built from every leaf in `help_map::registered_leaves()`
//! and asserting the result is a real, dispatchable command rather than a
//! `Help`/`UsageError` fallback -- so a future MCP-tool parity check (which
//! must consume this same tree, per RAL-301) can trust it as ground truth.
//!
//! This does not (yet) fully "generate" `help_map.rs` from the `Command`
//! enum -- the tree's descriptions and flag semantics have no source of
//! truth outside `help_map.rs` itself, so literal codegen would need a new
//! macro-based command-registry refactor across every `commands/*.rs` file
//! (out of scope for this pass). What this test guarantees instead: every
//! path `help_map.rs` claims is callable really is one `commands::parse_args`
//! recognizes, today and on every future change -- the actual failure mode
//! the ticket's "can already drift... independent of this ticket" note
//! warns about.

use ralphus_cli::commands;
use ralphus_cli::help_map;

/// A handful of leaves whose "which flag(s) are actually required" business
/// rule cannot be derived from `help_map.rs`'s chip data at all -- every
/// option chip in this crate's convention looks equally optional (unlike
/// positionals, which mark themselves `, optional]` when they are), so a
/// mutually-exclusive boolean pair like `review squash`'s `--on`/`--off`
/// ("requires exactly one of") has no chip-level signal to pick from. Kept
/// short and path-scoped on purpose: this is a deliberate, documented
/// exception list, not a general escape hatch -- if it grows past a couple
/// of entries, the chip format itself needs a "required" marker instead.
const BUSINESS_RULE_ONLY_EXTRA_ARGS: &[(&[&str], &[&str])] = &[(&["review", "squash"], &["--on"])];

/// Picks a dummy value for one `[hint]`/`[a|b]`/`[name=value...]` chip.
/// `"1"` parses as a valid `str`, `integer`, and `float` alike, so it never
/// trips a type-conversion error unrelated to the path-routing this test
/// actually checks; a literal-choice chip (`forge [github|gitlab]`) instead
/// gets its first listed literal, since some commands validate an enum-shaped
/// value against its exact allowed set before returning a real command; a
/// `key=value`-shaped chip (`--input [name=value...]`) gets a value that
/// itself contains `=`, since some commands split on it eagerly.
fn dummy_value_for(chip: &str) -> String {
    let Some(inner) = chip
        .split('[')
        .nth(1)
        .and_then(|rest| rest.split(']').next())
    else {
        return "1".to_string();
    };
    if inner.contains('|') {
        return inner.split('|').next().unwrap_or("1").to_string();
    }
    if inner.starts_with("name=value") {
        return "x=1".to_string();
    }
    "1".to_string()
}

/// Builds a synthetic argv for a leaf path: the path itself, one dummy value
/// per positional chip (skipping only ones explicitly marked `, optional]`
/// -- a repeatable `...]` positional still gets exactly one, since several
/// commands (`queue reorder`, `queue set-position`) require at least one
/// despite being repeatable), one `--flag value` pair per value-taking
/// option chip (every option chip in this crate's convention is written the
/// same way whether the underlying flag is actually required or not, so
/// filling all of them is the only way to reach commands that gate on a
/// required flag -- see `commands/queue.rs`'s `--to`, `commands/cell.rs`'s
/// `--from`, etc.), plus any [`BUSINESS_RULE_ONLY_EXTRA_ARGS`] override.
fn dummy_argv(path: &[&str], positionals: &[&str], options: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = path.iter().map(|s| (*s).to_string()).collect();
    for chip in positionals {
        if chip.contains(", optional]") {
            continue;
        }
        argv.push(dummy_value_for(chip));
    }
    for chip in options {
        if !chip.contains('[') {
            continue; // a bare boolean flag -- filling it is never required to reach a leaf.
        }
        let flag = chip.split(' ').next().unwrap_or(chip);
        argv.push(flag.to_string());
        argv.push(dummy_value_for(chip));
    }
    if let Some((_, extra)) = BUSINESS_RULE_ONLY_EXTRA_ARGS
        .iter()
        .find(|(p, _)| *p == path)
    {
        argv.extend(extra.iter().map(|s| (*s).to_string()));
    }
    argv
}

/// True if `debug` (a `{:?}`-formatted `Command`/`*Command`) is the
/// catch-all fallback shape every group in this crate uses for "didn't
/// recognize this" -- `UsageError(..)` (unknown subcommand or bad/missing
/// args) or a bare `Help` leaf (fell all the way back to the group's help
/// screen instead of matching a real leaf).
fn looks_unrecognized(debug: &str) -> bool {
    debug.contains("UsageError") || debug == "Help" || debug.ends_with("(Help)")
}

#[test]
fn every_help_map_leaf_is_dispatchable_by_commands_parse_args() {
    let mut failures = Vec::new();
    for (path, node) in help_map::registered_leaves() {
        if path.is_empty() {
            continue; // bare `ralphus` is the (non-leaf) help screen, not a command.
        }
        let argv = dummy_argv(&path, node.positionals, node.options);
        let cmd = commands::parse_args(&argv);
        let debug = format!("{cmd:?}");
        if looks_unrecognized(&debug) {
            failures.push(format!("{path:?} (argv {argv:?}) -> {debug}"));
        }
    }
    assert!(
        failures.is_empty(),
        "help_map.rs advertises these leaf commands, but commands::parse_args does not \
         recognize them -- the two have drifted out of sync:\n{}",
        failures.join("\n")
    );
}
