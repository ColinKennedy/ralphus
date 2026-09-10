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

use std::io::{Read as _, Write as _};

use opentelemetry::trace::SpanKind;
use ralphus_runner::execute::{preflight_agent, run_cell};
use ralphus_runner::spec::{CellResult, CellSpec};
use ralphus_runner::{config, otel};

/// Must match `daemon/src/runner.rs::TMUX_DONE_MARKER`.
const TMUX_DONE_MARKER: &str = "RALPHUS_TMUX_DONE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpCommand {
    Send,
    Preflight,
    PipeSink,
    License,
    Version,
}

impl HelpCommand {
    #[cfg(test)]
    fn name(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Preflight => "preflight",
            Self::PipeSink => "pipe-sink",
            Self::License => "license",
            Self::Version => "version",
        }
    }
}

#[cfg(test)]
const HELP_COMMANDS: &[HelpCommand] = &[
    HelpCommand::Send,
    HelpCommand::Preflight,
    HelpCommand::PipeSink,
    HelpCommand::License,
    HelpCommand::Version,
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help(Option<HelpCommand>),
    Version,
    License,
    Send {
        spec_path: Option<String>,
        result_file: Option<String>,
    },
    Preflight {
        agent: String,
        executable: Option<String>,
    },
    PipeSink {
        out: Option<String>,
        max_bytes: Option<u64>,
    },
}

fn main() -> std::process::ExitCode {
    // Touches the obfuscated embedded LICENSE (RAL-236) so thin-LTO release
    // builds don't strip it as dead code ahead of the `ralphus license`
    // subcommand landing.
    std::hint::black_box(ralphus_core::license::embedded_license());
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&raw_args) {
        Command::Help(command) => print_help(command),
        Command::Version => print_version(),
        Command::License => print_license(),
        Command::Send {
            spec_path,
            result_file,
        } => send(spec_path.as_deref(), result_file.as_deref()),
        Command::Preflight { agent, executable } => preflight(&agent, executable.as_deref()),
        Command::PipeSink { out, max_bytes } => pipe_sink(out.as_deref(), max_bytes),
    }
}

fn preflight(agent: &str, executable: Option<&str>) -> std::process::ExitCode {
    match preflight_agent(agent, executable, false) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Default cap on how much raw pane output `pipe_sink` will persist for a
/// single invocation (RAL-397 Phase 2B) — a safety valve against a single
/// runaway cell filling disk, independent of the daemon-side terminal-log
/// retention policy (which prunes across attempts/time, not within one).
/// 256 MiB is generous for even a very verbose build/test run while still
/// being a real bound; `--max-bytes` overrides it for callers that want a
/// different budget (e.g. Phase 2C/2H's configured value).
const DEFAULT_PIPE_SINK_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// `pipe-sink`: the portable target `Tmux::pipe_pane` (daemon/src/tmux.rs)
/// points a psmux/tmux `pipe-pane -o` at, instead of a shell one-liner —
/// this binary starts far faster than spawning an interpreter, and reusing
/// the already-shipped `ralphus-runner` executable needs no extra bundled
/// dependency (RAL-397 Phase 2). Reads raw bytes from stdin (never
/// line-splits — ANSI escape sequences do not respect line boundaries) and
/// appends them to `out`, flushing after every read so a poll-based reader
/// on the other end never waits longer than one read cycle to see new data.
///
/// Once `max_bytes` (or [`DEFAULT_PIPE_SINK_MAX_BYTES`] when unset) worth of
/// output has been written, further bytes are silently dropped rather than
/// written — stdin is still drained so the pane's own writes never block —
/// and a single truncation marker line is appended once, so a reader of the
/// file can tell the record is incomplete rather than assuming it saw
/// everything. Runs until stdin reaches EOF (the pane's process ended) or is
/// explicitly stopped (`Tmux::stop_pipe_pane`, which ends this process's
/// stdin).
fn pipe_sink(out_path: Option<&str>, max_bytes: Option<u64>) -> std::process::ExitCode {
    let Some(path) = out_path else {
        eprintln!("ralphus-runner pipe-sink: --out <path> is required");
        return std::process::ExitCode::FAILURE;
    };
    let cap = max_bytes.unwrap_or(DEFAULT_PIPE_SINK_MAX_BYTES);
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("ralphus-runner pipe-sink: could not open {path}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 8192];
    let mut written: u64 = 0;
    let mut truncated_marker_written = false;
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprintln!("ralphus-runner pipe-sink: error reading stdin: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
        if written < cap {
            let remaining = (cap - written) as usize;
            let take = remaining.min(n);
            if let Err(e) = file.write_all(&buf[..take]) {
                eprintln!("ralphus-runner pipe-sink: error writing {path}: {e}");
                return std::process::ExitCode::FAILURE;
            }
            written += take as u64;
        }
        if written >= cap && !truncated_marker_written {
            truncated_marker_written = true;
            let _ = writeln!(
                file,
                "\n[ralphus-runner pipe-sink: truncated at {cap} bytes]"
            );
        }
        let _ = file.flush();
    }
    std::process::ExitCode::SUCCESS
}

/// `help`/`version`/`license` are ordinary CLI-facing output, not the
/// daemon<->runner wire contract that the reserved stdout channel (see
/// `finish` below) exists to protect, hence the local opt-out from the
/// workspace-wide `clippy::print_stdout = "deny"`.
#[allow(clippy::print_stdout)]
fn print_help(command: Option<HelpCommand>) -> std::process::ExitCode {
    print!("{}", usage(command));
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

    // RAL-288 Stage 5: Cartographer-only, not a plain `eprintln!` -- see
    // `execute.rs::log_llm_start`'s doc comment for why (no counterpart
    // existed before Stage 5; a bare print would only ever have lived in the
    // vanishing tmux pane).
    ralphus_runner::cartographer::emit(
        "runner",
        "invoked",
        "info",
        ralphus_runner::cartographer::EventContext {
            squad_id: Some(&spec.squad_id),
            cell_id: Some(&spec.cell_id),
            task: Some(&spec.task),
        },
        serde_json::json!({"agent": spec.agent, "model": spec.model, "proof": spec.proof}),
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
    if let Some(command) = requested_help(args) {
        return Command::Help(command);
    }
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => Command::Version,
        Some("help") => Command::Help(None),
        Some(name) => match registered_command(name) {
            Some(HelpCommand::Version) => Command::Version,
            Some(HelpCommand::License) => Command::License,
            Some(HelpCommand::Send) => parse_send_args(&args[1..]),
            Some(HelpCommand::Preflight) => parse_preflight_args(&args[1..]),
            Some(HelpCommand::PipeSink) => parse_pipe_sink_args(&args[1..]),
            None => parse_send_args(args),
        },
        None => parse_send_args(args),
    }
}

fn registered_command(name: &str) -> Option<HelpCommand> {
    match name {
        "send" => Some(HelpCommand::Send),
        "preflight" => Some(HelpCommand::Preflight),
        "pipe-sink" => Some(HelpCommand::PipeSink),
        "license" => Some(HelpCommand::License),
        "version" => Some(HelpCommand::Version),
        _ => None,
    }
}

/// Splits `--out <path>` and an optional `--max-bytes <n>` out of a
/// `pipe-sink` invocation (RAL-397 Phase 2B).
fn parse_pipe_sink_args(args: &[String]) -> Command {
    let mut out = None;
    let mut max_bytes = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--out" if index + 1 < args.len() => {
                out = Some(args[index + 1].clone());
                index += 2;
            }
            "--max-bytes" if index + 1 < args.len() => {
                max_bytes = args[index + 1].parse().ok();
                index += 2;
            }
            _ => index += 1,
        }
    }
    Command::PipeSink { out, max_bytes }
}

fn parse_preflight_args(args: &[String]) -> Command {
    let mut agent = None;
    let mut executable = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--agent" if index + 1 < args.len() => {
                agent = Some(args[index + 1].clone());
                index += 2;
            }
            "--executable" if index + 1 < args.len() => {
                executable = Some(args[index + 1].clone());
                index += 2;
            }
            _ => index += 1,
        }
    }
    Command::Preflight {
        agent: agent.unwrap_or_default(),
        executable,
    }
}

fn requested_help(args: &[String]) -> Option<Option<HelpCommand>> {
    let before_separator: Vec<&String> =
        args.iter().take_while(|arg| arg.as_str() != "--").collect();
    if !before_separator
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return None;
    }
    let command = before_separator
        .first()
        .and_then(|arg| registered_command(arg.as_str()));
    Some(command)
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

fn usage(command: Option<HelpCommand>) -> String {
    match command {
        Some(HelpCommand::Send) => "ralphus-runner send -- Execute one cell specification.\n\nUSAGE:\n    ralphus-runner send [spec.json] [--result-file <path>]\n\nARGUMENTS:\n    spec.json             Optional JSON file; stdin is used when omitted\n\nOPTIONS:\n    --result-file <path>  Write the CellResult JSON to this path\n    -h, --help            Print help and exit\n".to_string(),
        Some(HelpCommand::Preflight) => "ralphus-runner preflight -- Check an agent launcher.\n\nUSAGE:\n    ralphus-runner preflight --agent <name> [--executable <path-or-command>]\n\nOPTIONS:\n    --agent <name>        Resolved backend name\n    --executable <value>  Optional backend launcher override\n    -h, --help            Print help and exit\n".to_string(),
        Some(HelpCommand::PipeSink) => "ralphus-runner pipe-sink -- Append raw stdin to a file (tmux/psmux pipe-pane target).\n\nUSAGE:\n    ralphus-runner pipe-sink --out <path> [--max-bytes <n>]\n\nOPTIONS:\n    --out <path>          File to append captured pane bytes to\n    --max-bytes <n>       Stop persisting after this many bytes (default 256 MiB)\n    -h, --help            Print help and exit\n".to_string(),
        Some(HelpCommand::License) => "ralphus-runner license -- Print the embedded LICENSE text.\n\nUSAGE:\n    ralphus-runner license\n\nOPTIONS:\n    -h, --help            Print help and exit\n".to_string(),
        Some(HelpCommand::Version) => "ralphus-runner version -- Print the runner version.\n\nUSAGE:\n    ralphus-runner version\n\nOPTIONS:\n    -h, --help            Print help and exit\n".to_string(),
        None => format!(
            "ralphus-runner {}\n\nUSAGE:\n    ralphus-runner <COMMAND> [ARGS...]\n\nCOMMANDS:\n    send              Execute one cell spec from stdin or a file path\n    preflight         Check whether an agent launcher is available\n    pipe-sink         Append raw stdin to a file (tmux/psmux pipe-pane target)\n    license           Print the embedded LICENSE text\n    version           Print version and exit\n    help              Print this message\n\nOPTIONS:\n    -h, --help        Print help and exit\n",
            ralphus_core::version()
        ),
    }
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
                // No `CellSpec` in scope here to attach squad/task/cell
                // context to -- the daemon's own forwarding fills those in
                // from what it already knows about this invocation (see
                // `daemon/src/runner.rs::forward_runner_event`'s fallback).
                ralphus_runner::cartographer::emit(
                    "runner",
                    "could not write result file",
                    "error",
                    ralphus_runner::cartographer::EventContext::default(),
                    serde_json::json!({"path": path, "error": e.to_string()}),
                );
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
        assert_eq!(parse_args(&v(&["help"])), Command::Help(None));
    }

    #[test]
    fn usage_mentions_send_and_license() {
        let text = usage(None);
        assert!(text.contains("send"));
        assert!(text.contains("license"));
    }

    #[test]
    fn help_precedes_send_validation_and_result_file_handling() {
        assert_eq!(
            parse_args(&v(&[
                "send",
                "missing.json",
                "--result-file",
                "out.json",
                "--help",
            ])),
            Command::Help(Some(HelpCommand::Send))
        );
        assert!(usage(Some(HelpCommand::Send)).contains("--result-file <path>"));
    }

    #[test]
    fn parse_pipe_sink_with_out_and_max_bytes() {
        assert_eq!(
            parse_args(&v(&[
                "pipe-sink",
                "--out",
                "transcript.raw",
                "--max-bytes",
                "1024"
            ])),
            Command::PipeSink {
                out: Some("transcript.raw".to_string()),
                max_bytes: Some(1024),
            }
        );
    }

    #[test]
    fn parse_pipe_sink_without_max_bytes_defaults_to_none() {
        assert_eq!(
            parse_args(&v(&["pipe-sink", "--out", "transcript.raw"])),
            Command::PipeSink {
                out: Some("transcript.raw".to_string()),
                max_bytes: None,
            }
        );
    }

    #[test]
    fn pipe_sink_without_out_flag_fails_fast() {
        assert_eq!(pipe_sink(None, None), std::process::ExitCode::FAILURE);
    }

    // `pipe_sink`'s actual read/write loop (byte fidelity, `--max-bytes` cap)
    // is exercised by `runner/tests/pipe_sink.rs` instead of here: it needs
    // to spawn the real compiled binary with piped process-level stdin,
    // which requires `CARGO_BIN_EXE_ralphus-runner` -- only set for files
    // under `tests/`, not for a bin crate's own inline `#[cfg(test)]` module.

    #[test]
    fn every_registered_runner_command_has_help() {
        for command in HELP_COMMANDS {
            let parsed = parse_args(&v(&[command.name(), "--bad", "--help"]));
            assert_eq!(parsed, Command::Help(Some(*command)));
            assert!(usage(Some(*command)).contains("-h, --help"));
        }
    }

    #[test]
    fn help_after_separator_is_not_intercepted() {
        assert!(matches!(
            parse_args(&v(&["send", "spec.json", "--", "--help"])),
            Command::Send { .. }
        ));
    }
}
