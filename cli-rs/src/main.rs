//! `ralphus` CLI binary, ported from `cli/src/ralphus/__main__.py::main`.
//! Extracts global `--json`/`--daemon-url` flags from anywhere in argv (see
//! `args::extract_global_opts`), parses the remaining tokens into a
//! [`ralphus_cli::commands::Command`], dispatches it, and exits with its
//! process exit code.
#![allow(clippy::print_stdout)] // This binary's stdout IS the CLI's product.

use ralphus_cli::args::extract_global_opts;
use ralphus_cli::commands::{dispatch, parse_args};

fn main() -> std::process::ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let (opts, args) = extract_global_opts(&raw_args);
    let cmd = parse_args(&args);
    eprintln!("ralphus [cli] {cmd:?}");
    let code = dispatch(cmd, &opts);
    match u8::try_from(code) {
        Ok(c) => std::process::ExitCode::from(c),
        Err(_) => std::process::ExitCode::FAILURE,
    }
}
