//! `ralphus-runner` binary: reads one `SessionSpec` JSON object (from stdin,
//! or a file path given as the first positional argument) and reports one
//! `SessionResult`. Spawned once per session by
//! `daemon/src/runner.rs::SubprocessRunner`, which always runs it tmux-
//! wrapped and always passes `--result-file <path>` (see `run_via_tmux_attempt`)
//! -- when the daemon can't read this process's stdout as a pipe (it lands in
//! a tmux pane instead), the result is written to that file instead of
//! printed, and a `RALPHUS_TMUX_DONE: <status>` sentinel line is printed to
//! stdout in its place for `daemon/src/runner.rs::pane_shows_done_sentinel`
//! to notice. Without `--result-file` (plain subprocess invocation), the
//! `SessionResult` JSON goes to stdout instead. Mirrors
//! `cli/src/ralphus/runner/__main__.py`'s `_parse_argv`/`_finish` exactly --
//! this is the wire contract the daemon already speaks.

use std::io::Read as _;

use opentelemetry::trace::SpanKind;
use ralphus_runner::execute::run_session;
use ralphus_runner::spec::{SessionResult, SessionSpec};
use ralphus_runner::{config, otel};

/// Must match `daemon/src/runner.rs::TMUX_DONE_MARKER`.
const TMUX_DONE_MARKER: &str = "RALPHUS_TMUX_DONE";

fn main() -> std::process::ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let (positional, result_file) = parse_args(&raw_args);

    let input = match read_spec_text(positional.first().map(String::as_str)) {
        Ok(s) => s,
        Err(e) => {
            return finish(
                &SessionResult::failed(format!("could not read session spec: {e}"), ""),
                result_file.as_deref(),
            );
        }
    };

    let spec = match SessionSpec::from_json(&input) {
        Ok(s) => s,
        Err(e) => {
            return finish(
                &SessionResult::failed(e.to_string(), ""),
                result_file.as_deref(),
            );
        }
    };

    eprintln!(
        "ralphus [runner] invoked run={} session={} agent={:?} model={:?} verify={}",
        spec.run_id, spec.session_id, spec.agent, spec.model, spec.verify
    );

    let runner_config = config::load(std::path::Path::new(&spec.cwd));
    let provider = otel::init("ralphus-runner");

    let result = run_traced(&spec, runner_config.keep_temporary_files);

    otel::shutdown(provider);
    finish(&result, result_file.as_deref())
}

/// Splits an optional `--result-file PATH` out of `args`, mirroring the old
/// Python `_parse_argv`. Returns the remaining positional args (the spec
/// file path, if any) and the result-file path (`None` when absent).
fn parse_args(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut rest = Vec::new();
    let mut result_file = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--result-file" && i + 1 < args.len() {
            result_file = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        rest.push(args[i].clone());
        i += 1;
    }
    (rest, result_file)
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

fn run_traced(spec: &SessionSpec, keep_temporary_files: bool) -> SessionResult {
    if spec.command.is_some() {
        // No LLM call for a command session -- no span needed.
        return run_session(spec, keep_temporary_files);
    }
    let root = otel::context_from_traceparent(spec.trace_context.as_deref());
    let span_name = if spec.verify {
        "llm.verify"
    } else {
        "llm.session"
    };
    let span = otel::start_span(span_name, &root, SpanKind::Client);
    span.set_attribute("run_id", spec.run_id.clone());
    span.set_attribute("session_id", spec.session_id.clone());
    span.set_attribute("agent", spec.agent.clone());

    let result = run_session(spec, keep_temporary_files);

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
fn finish(result: &SessionResult, result_file: Option<&str>) -> std::process::ExitCode {
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
