//! Thin CLI entry point -- see `src/lib.rs` for the crate's actual scope and
//! design docs.
#![allow(clippy::print_stdout)]

use std::io::Read;

use ralphus_ssh_provider::{UNIMPLEMENTED_VERBS, exec, ping, protocol};

struct Args {
    verb: String,
    uri: String,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let verb = argv
        .first()
        .cloned()
        .ok_or_else(|| "missing verb argument".to_string())?;
    let mut uri = String::new();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--uri" => {
                uri = argv
                    .get(i + 1)
                    .cloned()
                    .ok_or_else(|| "--uri requires a value".to_string())?;
                i += 2;
            }
            // Handle-scoped flags belong to the async-`exec` verbs this
            // provider does not implement; accepted and ignored rather than
            // rejected so a future daemon that always passes them stays
            // compatible with this provider's synchronous-only `exec`.
            "--handle" | "--since" => {
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }
    Ok(Args { verb, uri })
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
            let config = exec::EffectiveConfig::from_env();
            match exec::run(&args.uri, &payload, &config) {
                Ok(result) => protocol::reply_exec_result(result),
                Err(e) => protocol::reply_err(e),
            }
        }
        "ping" => {
            let config = exec::EffectiveConfig::from_env();
            match ping::run(&args.uri, &config) {
                Ok(detail) => protocol::reply_ping_ok(detail),
                Err(e) => protocol::reply_err(e),
            }
        }
        v if UNIMPLEMENTED_VERBS.contains(&v) => {
            protocol::reply_err(format!(
                "ralphus-ssh-provider implements the exec/ping verbs only (RAL-200); \
                 {v:?} is not implemented -- see RAL-201 for daemon-side dispatch of \
                 the remaining verbs"
            ));
        }
        v => protocol::reply_err(format!("unknown verb {v:?}")),
    }
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
    }

    #[test]
    fn missing_verb_is_an_error() {
        assert!(parse_args(&[]).is_err());
    }

    #[test]
    fn handle_and_since_flags_are_accepted_and_ignored() {
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
    }

    #[test]
    fn missing_uri_value_is_an_error() {
        assert!(parse_args(&["exec".to_string(), "--uri".to_string()]).is_err());
    }
}
