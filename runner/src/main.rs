//! `ralphus-runner` binary: `send` reads one `CellSpec` JSON object (from
//! stdin, or a file path given as the first positional argument) and reports
//! one `CellResult`. Spawned once per cell by
//! `daemon/src/runner.rs::SubprocessRunner`, which always runs it tmux-
//! wrapped and always passes `send --result-file <path>` (see
//! `run_via_tmux_attempt`) -- when the daemon can't read this process's
//! stdout as a pipe (it lands in a tmux pane instead), the result is written
//! to that file instead of printed, and a `RALPHUS_TMUX_DONE: <status>`
//! sentinel line is printed to stdout in its place for
//! `daemon/src/runner.rs::pane_shows_done_sentinel` to notice. Without
//! `--result-file` (plain subprocess invocation), the `CellResult` JSON goes
//! to stdout instead. This is the wire contract the daemon already speaks.

use std::io::Read as _;

use opentelemetry::trace::SpanKind;
use ralphus_runner::execute::run_cell;
use ralphus_runner::spec::{CellResult, CellSpec};
use ralphus_runner::{config, otel};

/// Must match `daemon/src/runner.rs::TMUX_DONE_MARKER`.
const TMUX_DONE_MARKER: &str = "RALPHUS_TMUX_DONE";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    License,
    Send {
        spec_path: Option<String>,
        result_file: Option<String>,
    },
}

fn main() -> std::process::ExitCode {
    // Touches the obfuscated embedded LICENSE (RAL-236) so thin-LTO release
    // builds don't strip it as dead code ahead of the `ralphus license`
    // subcommand landing.
    std::hint::black_box(ralphus_core::license::embedded_license());
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&raw_args) {
        Command::Help => print_help(),
        Command::Version => print_version(),
        Command::License => print_license(),
        Command::Send {
            spec_path,
            result_file,
        } => send(spec_path.as_deref(), result_file.as_deref()),
    }
}

/// `help`/`version`/`license` are ordinary CLI-facing output, not the
/// daemon<->runner wire contract that the reserved stdout channel (see
/// `finish` below) exists to protect, hence the local opt-out from the
/// workspace-wide `clippy::print_stdout = "deny"`.
#[allow(clippy::print_stdout)]
fn print_help() -> std::process::ExitCode {
    print!("{}", usage());
    std::process::ExitCode::SUCCESS
}

#[allow(clippy::print_stdout)] // see `print_help`
fn print_version() -> std::process::ExitCode {
    println!("ralphus-runner {}", ralphus_core::version());
    std::process::ExitCode::SUCCESS
}

#[allow(clippy::print_stdout)] // see `print_help`
fn print_license() -> std::process::ExitCode {
    print!("{}", ralphus_core::license::embedded_license());
    std::process::ExitCode::SUCCESS
}

fn send(spec_path: Option<&str>, result_file: Option<&str>) -> std::process::ExitCode {
    let input = match read_spec_text(spec_path) {
        Ok(s) => s,
        Err(e) => {
            return finish(
                &CellResult::failed(format!("could not read cell spec: {e}"), ""),
                result_file,
            );
        }
    };

    let spec = match CellSpec::from_json(&input) {
        Ok(s) => s,
        Err(e) => {
            return finish(&CellResult::failed(e.to_string(), ""), result_file);
        }
    };

    eprintln!(
        "ralphus [runner] invoked squad={} cell={} agent={:?} model={:?} proof={}",
        spec.squad_id, spec.cell_id, spec.agent, spec.model, spec.proof
    );

    let runner_config = config::load(std::path::Path::new(&spec.cwd));
    let provider = otel::init("ralphus-runner");

    let result = run_traced(&spec, runner_config.keep_temporary_files);

    otel::shutdown(provider);
    finish(&result, result_file)
}

/// Parse the runner CLI. `send` is the explicit command surface, but the
/// historical "implicit send" argv shape remains accepted as a compatibility
/// fallback for callers that still invoke `ralphus-runner [spec]`.
fn parse_args(args: &[String]) -> Command {
    match args.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => Command::Version,
        Some("license") => Command::License,
        Some("--help" | "-h" | "help") => Command::Help,
        Some("send") => parse_send_args(&args[1..]),
        _ => parse_send_args(args),
    }
}

/// Splits an optional `--result-file PATH` out of a `send` invocation,
/// mirroring the old Python `_parse_argv`.
fn parse_send_args(args: &[String]) -> Command {
    let mut positionals = Vec::new();
    let mut result_file = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--result-file" && i + 1 < args.len() {
            result_file = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        positionals.push(args[i].clone());
        i += 1;
    }
    Command::Send {
        spec_path: positionals.into_iter().next(),
        result_file,
    }
}

fn usage() -> String {
    format!(
        "ralphus-runner {}\n\nUSAGE:\n    ralphus-runner send [spec.json] [--result-file <path>]\n    ralphus-runner license\n    ralphus-runner version\n    ralphus-runner help\n\nCOMMANDS:\n    send              Execute one cell spec from stdin or a file path\n    license           Print the embedded LICENSE text\n    version           Print version and exit\n    help              Print this message\n",
        ralphus_core::version()
    )
}

/// Reads the spec JSON from `path` when given, else from stdin -- mirrors
/// the old Python `_read_spec_text`.
fn read_spec_text(path: Option<&str>) -> std::io::Result<String> {
    match path {
        Some(p) => std::fs::read_to_string(p),
        None => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            Ok(input)
        }
    }
}

fn run_traced(spec: &CellSpec, keep_temporary_files: bool) -> CellResult {
    if spec.command.is_some() {
        // No LLM call for a command cell -- no span needed.
        return run_cell(spec, keep_temporary_files);
    }
    let root = otel::context_from_traceparent(spec.trace_context.as_deref());
    let span_name = if spec.proof { "llm.proof" } else { "llm.cell" };
    let span = otel::start_span(span_name, &root, SpanKind::Client);
    span.set_attribute("squad_id", spec.squad_id.clone());
    span.set_attribute("cell_id", spec.cell_id.clone());
    span.set_attribute("agent", spec.agent.clone());

    let result = run_cell(spec, keep_temporary_files);

    if result.ok() {
        span.set_status(opentelemetry::trace::Status::Ok);
    } else {
        span.set_status(opentelemetry::trace::Status::error(
            result.error.clone().unwrap_or_default(),
        ));
    }
    result
}

/// Reports `result`: written to `result_file` plus the `RALPHUS_TMUX_DONE`
/// stdout sentinel when tmux-wrapped, or as the one JSON line on stdout
/// otherwise -- the reserved daemon<->runner channel
/// (`clippy::print_stdout = "deny"` workspace-wide; these are the two
/// legitimate writers, hence the local opt-out).
#[allow(clippy::print_stdout)]
fn finish(result: &CellResult, result_file: Option<&str>) -> std::process::ExitCode {
    match result_file {
        Some(path) => {
            if let Err(e) = std::fs::write(path, result.to_json()) {
                eprintln!("ralphus [runner] could not write result file {path}: {e}");
            }
            println!("{TMUX_DONE_MARKER}: {}", result.status);
        }
        None => println!("{}", result.to_json()),
    }
    if result.ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parse_send_subcommand_with_result_file() {
        assert_eq!(
            parse_args(&v(&["send", "spec.json", "--result-file", "result.json"])),
            Command::Send {
                spec_path: Some("spec.json".to_string()),
                result_file: Some("result.json".to_string()),
            }
        );
    }

    #[test]
    fn parse_legacy_implicit_send_shape_still_works() {
        assert_eq!(
            parse_args(&v(&["spec.json", "--result-file", "result.json"])),
            Command::Send {
                spec_path: Some("spec.json".to_string()),
                result_file: Some("result.json".to_string()),
            }
        );
    }

    #[test]
    fn parse_license_and_help_commands() {
        assert_eq!(parse_args(&v(&["license"])), Command::License);
        assert_eq!(parse_args(&v(&["help"])), Command::Help);
    }

    #[test]
    fn usage_mentions_send_and_license() {
        let text = usage();
        assert!(text.contains("send"));
        assert!(text.contains("license"));
    }
}
