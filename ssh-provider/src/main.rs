//! Thin CLI entry point -- see `src/lib.rs` for the crate's actual scope and
//! design docs.
#![allow(clippy::print_stdout)]

use std::io::Read;

use ralphus_ssh_provider::{
    UNIMPLEMENTED_VERBS, capabilities, cleanup, exec, fileops, job, ping, protocol, provision,
    terminal,
};

struct Args {
    verb: String,
    uri: String,
    ssh_config_file: Option<String>,
    handle: Option<String>,
    since: i64,
    /// `terminal` only: the remote command to run under the allocated pty.
    command: Option<String>,
    /// `terminal` only: initial terminal size.
    cols: u16,
    lines: u16,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut verb = None;
    let mut uri = String::new();
    let mut ssh_config_file = None;
    let mut handle = None;
    let mut since = 0_i64;
    let mut command = None;
    let mut cols = 80_u16;
    let mut lines = 24_u16;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--uri" => {
                uri = argv
                    .get(i + 1)
                    .cloned()
                    .ok_or_else(|| "--uri requires a value".to_string())?;
                i += 2;
            }
            "--ssh-config" => {
                ssh_config_file = Some(
                    argv.get(i + 1)
                        .cloned()
                        .ok_or_else(|| "--ssh-config requires a value".to_string())?,
                );
                i += 2;
            }
            // Handle-scoped flags are shared by the async job verbs.
            "--handle" => {
                handle = Some(
                    argv.get(i + 1)
                        .cloned()
                        .ok_or_else(|| "--handle requires a value".to_string())?,
                );
                i += 2;
            }
            "--since" => {
                since = argv
                    .get(i + 1)
                    .ok_or_else(|| "--since requires a value".to_string())?
                    .parse()
                    .map_err(|_| "--since must be an integer".to_string())?;
                i += 2;
            }
            "--command" => {
                command = Some(
                    argv.get(i + 1)
                        .cloned()
                        .ok_or_else(|| "--command requires a value".to_string())?,
                );
                i += 2;
            }
            "--cols" => {
                cols = argv
                    .get(i + 1)
                    .ok_or_else(|| "--cols requires a value".to_string())?
                    .parse()
                    .map_err(|_| "--cols must be a positive integer".to_string())?;
                i += 2;
            }
            "--lines" => {
                lines = argv
                    .get(i + 1)
                    .ok_or_else(|| "--lines requires a value".to_string())?
                    .parse()
                    .map_err(|_| "--lines must be a positive integer".to_string())?;
                i += 2;
            }
            value if !value.starts_with('-') && verb.is_none() => {
                verb = Some(value.to_string());
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(Args {
        verb: verb.ok_or_else(|| "missing verb argument".to_string())?,
        uri,
        ssh_config_file,
        handle,
        since,
        command,
        cols,
        lines,
    })
}

fn read_stdin() -> String {
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            protocol::reply_err(format!("could not parse arguments: {e}"));
            return;
        }
    };

    match args.verb.as_str() {
        "exec" => {
            let payload = read_stdin();
            let mut config = exec::EffectiveConfig::from_env();
            if args.ssh_config_file.is_some() {
                config.ssh_config_file = args.ssh_config_file.clone();
            }
            let result = if config.target_runner_config.is_some() {
                job::start(&args.uri, &payload, &config).map(protocol::reply_exec_handle)
            } else {
                exec::run(&args.uri, &payload, &config).map(protocol::reply_exec_result)
            };
            if let Err(e) = result {
                protocol::reply_err(e);
            }
        }
        "status" => {
            let config = exec_config(&args);
            match required_handle(&args).and_then(|handle| job::status(&args.uri, handle, &config))
            {
                Ok(status) => protocol::reply_status(status.state, status.result),
                Err(e) => protocol::reply_err(e),
            }
        }
        "stream" => {
            let config = exec_config(&args);
            match required_handle(&args)
                .and_then(|handle| job::stream(&args.uri, handle, args.since, &config))
            {
                Ok(stream) => {
                    for line in stream.output.lines() {
                        if line.starts_with(ralphus_ssh_provider::EVENT_MARKER) {
                            eprintln!("{line}");
                        }
                    }
                    protocol::reply_stream(stream.output, stream.next);
                }
                Err(e) => protocol::reply_err(e),
            }
        }
        "cancel" => {
            let config = exec_config(&args);
            match required_handle(&args).and_then(|handle| job::cancel(&args.uri, handle, &config))
            {
                Ok(()) => protocol::reply_ok(),
                Err(e) => protocol::reply_err(e),
            }
        }
        "job-cleanup" => {
            let config = exec_config(&args);
            match required_handle(&args).and_then(|handle| job::cleanup(&args.uri, handle, &config))
            {
                Ok(()) => protocol::reply_ok(),
                Err(e) => protocol::reply_err(e),
            }
        }
        "ping" => {
            let mut config = exec::EffectiveConfig::from_env();
            if args.ssh_config_file.is_some() {
                config.ssh_config_file = args.ssh_config_file.clone();
            }
            match ping::run(&args.uri, &config) {
                Ok(detail) => protocol::reply_ping_ok(detail),
                Err(e) => protocol::reply_err(e),
            }
        }
        "capabilities" => {
            let config = exec_config(&args);
            protocol::reply_capabilities(capabilities::run(&args.uri, &config));
        }
        "terminal" => {
            // No `protocol::reply` here by design -- see `terminal`'s module
            // docs. Any error is reported on *stderr* (never stdout, which is
            // the raw terminal byte stream from the moment this verb starts)
            // and this process exits non-zero so the daemon can tell the
            // relay never even connected.
            let config = exec_config(&args);
            let Some(command) = args.command.clone() else {
                eprintln!("terminal requires --command");
                std::process::exit(1);
            };
            match terminal::run(&args.uri, &command, args.cols, args.lines, &config) {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        "provision" => {
            let payload = read_stdin();
            let mut config = exec::EffectiveConfig::from_env();
            if args.ssh_config_file.is_some() {
                config.ssh_config_file = args.ssh_config_file.clone();
            }
            match provision::run(&args.uri, &payload, &config) {
                Ok(workspace) => protocol::reply_provision_ok(workspace),
                Err(e) => protocol::reply_err(e),
            }
        }
        "read-file" => {
            let payload = read_stdin();
            let config = exec_config(&args);
            match fileops::read_file(&args.uri, &payload, &config) {
                Ok(content) => protocol::reply_file_content(content),
                Err(e) => protocol::reply_err(e),
            }
        }
        "write-file" => {
            let payload = read_stdin();
            let config = exec_config(&args);
            match fileops::write_file(&args.uri, &payload, &config) {
                Ok(()) => protocol::reply_ok(),
                Err(e) => protocol::reply_err(e),
            }
        }
        "remove-path" => {
            let payload = read_stdin();
            let config = exec_config(&args);
            match fileops::remove_path(&args.uri, &payload, &config) {
                Ok(()) => protocol::reply_ok(),
                Err(e) => protocol::reply_err(e),
            }
        }
        "run" => {
            let payload = read_stdin();
            let config = exec_config(&args);
            match fileops::run(&args.uri, &payload, &config) {
                Ok((stdout, exit_code)) => protocol::reply_run_result(stdout, exit_code),
                Err(e) => protocol::reply_err(e),
            }
        }
        "cleanup" => {
            let payload = read_stdin();
            let config = exec_config(&args);
            match cleanup::run(&args.uri, &payload, &config) {
                Ok(removed) => protocol::reply_cleanup_ok(removed),
                Err(e) => protocol::reply_err(e),
            }
        }
        v if UNIMPLEMENTED_VERBS.contains(&v) => {
            protocol::reply_err(format!(
                "ralphus-ssh-provider does not implement {v:?}; see \
                 docs/machine-providers.md for its supported verbs"
            ));
        }
        v => protocol::reply_err(format!("unknown verb {v:?}")),
    }
}

fn exec_config(args: &Args) -> exec::EffectiveConfig {
    let mut config = exec::EffectiveConfig::from_env();
    if args.ssh_config_file.is_some() {
        config.ssh_config_file = args.ssh_config_file.clone();
    }
    config
}

fn required_handle(args: &Args) -> Result<&str, String> {
    args.handle
        .as_deref()
        .filter(|handle| !handle.trim().is_empty())
        .ok_or_else(|| format!("{} requires --handle", args.verb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verb_and_uri() {
        let args = parse_args(&[
            "exec".to_string(),
            "--uri".to_string(),
            "alice@host".to_string(),
        ])
        .unwrap();
        assert_eq!(args.verb, "exec");
        assert_eq!(args.uri, "alice@host");
        assert_eq!(args.ssh_config_file, None);
        assert_eq!(args.handle, None);
        assert_eq!(args.since, 0);
    }

    #[test]
    fn parses_provider_options_before_the_verb() {
        let args = parse_args(&[
            "--ssh-config".to_string(),
            "C:/fixture/ssh_config".to_string(),
            "ping".to_string(),
            "--uri".to_string(),
            "fixture".to_string(),
        ])
        .unwrap();
        assert_eq!(args.verb, "ping");
        assert_eq!(args.uri, "fixture");
        assert_eq!(
            args.ssh_config_file.as_deref(),
            Some("C:/fixture/ssh_config")
        );
    }

    #[test]
    fn missing_verb_is_an_error() {
        assert!(parse_args(&[]).is_err());
    }

    #[test]
    fn handle_and_since_flags_are_parsed() {
        let args = parse_args(&[
            "status".to_string(),
            "--uri".to_string(),
            "host".to_string(),
            "--handle".to_string(),
            "h1".to_string(),
            "--since".to_string(),
            "0".to_string(),
        ])
        .unwrap();
        assert_eq!(args.verb, "status");
        assert_eq!(args.uri, "host");
        assert_eq!(args.handle.as_deref(), Some("h1"));
        assert_eq!(args.since, 0);
    }

    #[test]
    fn missing_uri_value_is_an_error() {
        assert!(parse_args(&["exec".to_string(), "--uri".to_string()]).is_err());
    }
}
