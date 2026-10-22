//! `ralphus-runner` binary: a thin shell around [`ralphus_runner::cli`], which
//! documents the command line and the daemon<->runner wire contract.

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ralphus_runner::cli::main_with_args(&args)
}
