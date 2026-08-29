//! `ralphus` CLI binary, ported from `cli/src/ralphus/__main__.py::main`.
//! Extracts global `--json`/`--daemon-url` flags from anywhere in argv (see
//! `args::extract_global_opts`), parses the remaining tokens into a
//! [`ralphus_cli::commands::Command`], dispatches it, and exits with its
//! process exit code.
#![allow(clippy::print_stdout)] // This binary's stdout IS the CLI's product.

use ralphus_cli::args::extract_global_opts;
use ralphus_cli::commands::{dispatch, parse_args};

fn main() -> std::process::ExitCode {
    // Touches the obfuscated embedded LICENSE (RAL-236) so thin-LTO release
    // builds don't strip it as dead code ahead of the `ralphus license`
    // subcommand landing.
    std::hint::black_box(ralphus_core::license::embedded_license());
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(help) = ralphus_cli::help_map::requested_help(&raw_args) {
        println!("{help}");
        return std::process::ExitCode::SUCCESS;
    }
    if raw_args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--version")
    {
        println!(
            "{} {}",
            ralphus_cli::program_name::resolve_program_name(),
            ralphus_core::version()
        );
        return std::process::ExitCode::SUCCESS;
    }
    if let Err(message) = ralphus_cli::help_map::validate_invocation(&raw_args) {
        println!("usage error: {message}");
        return std::process::ExitCode::from(2);
    }
    let (opts, args) = extract_global_opts(&raw_args);
    let cmd = parse_args(&args);
    eprintln!(
        "{} [cli] {cmd:?}",
        ralphus_cli::program_name::resolve_program_name()
    );
    let code = dispatch(cmd, &opts);
    match u8::try_from(code) {
        Ok(c) => std::process::ExitCode::from(c),
        Err(_) => std::process::ExitCode::FAILURE,
    }
}
