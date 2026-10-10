//! The optional `channel` verb (RAL-185 D7): one long-lived `ssh` session that
//! serves many `run` requests, instead of one `ssh` process and one connection
//! handshake per command.
//!
//! A review merge issues ~23 `run`s. Without a channel each costs a provider
//! process, an `ssh` process and a full handshake -- and a Windows daemon host
//! cannot amortise that with `ControlMaster`, which Windows OpenSSH lacks.
//! With one, the daemon spawns this verb once per machine, streams
//! newline-delimited `run` payloads in, and reads one newline-delimited JSON
//! reply per request out (`daemon/src/channel.rs`).
//!
//! The session is `ssh <target> -- sh`: each request becomes a script written
//! to the remote shell's stdin, built by the same [`fileops::run_script`] the
//! one-shot `run` verb uses (identical validation, quoting and root-scoping),
//! followed by a per-request marker line carrying the command's exit code.
//!
//! Failure policy matches the contract: a *request* problem (malformed
//! payload, `cwd` outside the remote root) is an `ok: false` reply and the
//! channel lives on; a *transport* problem (ssh died, a write failed) ends the
//! verb without answering, which the daemon reads as "channel broken" and
//! falls back to a one-shot spawn for that command.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use serde_json::json;

use crate::exec::EffectiveConfig;
use crate::protocol::PROTOCOL_VERSION;
use crate::{fileops, ssh, uri};

/// Serve `run` requests from stdin to stdout over one `ssh` session to `uri`.
///
/// # Errors
/// The target cannot be parsed, `ssh` cannot be started, or the session dies.
pub fn serve(uri: &str, config: &EffectiveConfig) -> Result<(), String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    let mut args = ssh::non_interactive_args(
        config.connect_timeout_secs,
        config.ssh_config_file.as_deref(),
    );
    // A silent dead peer is noticed in about 90 s instead of hanging a merge.
    args.extend([
        "-o".to_string(),
        "ServerAliveInterval=30".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        target.target_string(),
        "--".to_string(),
        "sh".to_string(),
    ]);
    let mut session = Command::new("ssh");
    session.args(args);
    serve_with(
        session,
        config,
        std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    )
}

/// One reply line: the `run` verb's envelope (`protocol::reply_run_result`).
fn reply_line(stdout: &str, exit_code: i64) -> String {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "ok": true,
        "stdout": stdout,
        "exit_code": exit_code,
    })
    .to_string()
}

/// One error reply line (`protocol::reply_err`).
fn error_line(message: &str) -> String {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "ok": false,
        "error": message,
    })
    .to_string()
}

/// [`serve`] over an arbitrary session command and I/O, so tests can run the
/// same loop against a local shell.
pub(crate) fn serve_with(
    mut session: Command,
    config: &EffectiveConfig,
    requests: impl BufRead,
    replies: &mut impl Write,
) -> Result<(), String> {
    let mut child = session
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start the channel session: {e}"))?;
    let mut shell_in = child.stdin.take().ok_or("channel session has no stdin")?;
    let shell_out = child.stdout.take().ok_or("channel session has no stdout")?;
    let shell_err = child.stderr.take().ok_or("channel session has no stderr")?;
    // What `ssh` said on stderr (a refused key, a host-key mismatch, ...): kept
    // so a session that dies explains why instead of just ending.
    let stderr_reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = std::io::Read::read_to_end(&mut BufReader::new(shell_err), &mut text);
        String::from_utf8_lossy(&text).into_owned()
    });
    // Output is read on its own thread so a dead session ends the wait.
    let (lines, output) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(shell_out);
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if lines.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let outcome = (|| -> Result<(), String> {
        for (index, request) in requests.lines().enumerate() {
            let request = request.map_err(|e| format!("could not read a request: {e}"))?;
            if request.trim().is_empty() {
                continue;
            }
            // A marker no command output can plausibly contain: it names this
            // process and request.
            let marker = format!("__RALPHUS_CHANNEL_EXIT_{}_{index}__:", std::process::id());
            let script = match fileops::run_script(&request, config, &marker, true) {
                Ok(script) => script,
                Err(message) => {
                    send(replies, &error_line(&message))?;
                    continue;
                }
            };
            writeln!(shell_in, "{script}")
                .and_then(|()| shell_in.flush())
                .map_err(|e| format!("channel session write failed: {e}"))?;

            let mut raw = String::new();
            let exit_code = loop {
                let line = output
                    .recv()
                    .map_err(|_| "channel session ended mid-request".to_string())?;
                let text = String::from_utf8_lossy(&line);
                if let Some(code) = text.strip_prefix(marker.as_str()) {
                    break code.trim().parse::<i64>().map_err(|_| {
                        format!("channel produced a non-numeric exit code {code:?}")
                    })?;
                }
                raw.push_str(&text);
            };
            // The script prints a newline before the marker so it always
            // starts a line; the one-shot verb trims it the same way.
            let stdout = raw.trim_end_matches('\n');
            send(replies, &reply_line(stdout, exit_code))?;
        }
        Ok(())
    })();

    // EOF on the session's stdin ends the remote `sh`, which ends `ssh`.
    drop(shell_in);
    let _ = child.kill();
    let status = child.wait().ok().and_then(|s| s.code());
    let said = stderr_reader.join().unwrap_or_default();
    outcome.map_err(|message| {
        if said.trim().is_empty() {
            message
        } else {
            format!(
                "{message}: {}",
                ssh::interpret_failure("ssh", status, &said)
            )
        }
    })
}

fn send(replies: &mut impl Write, line: &str) -> Result<(), String> {
    writeln!(replies, "{line}")
        .and_then(|()| replies.flush())
        .map_err(|e| format!("could not write a reply: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn local_shell() -> Option<Command> {
        Command::new("sh")
            .args(["-c", "true"])
            .status()
            .ok()?
            .success()
            .then(|| Command::new("sh"))
    }

    fn request(command: &str) -> String {
        json!({"cwd": "/", "program": "", "args": [command]}).to_string()
    }

    fn run(requests: &[String]) -> (Result<(), String>, Vec<serde_json::Value>) {
        let session = local_shell().expect("a POSIX sh on PATH");
        let input = Cursor::new(requests.join("\n") + "\n");
        let mut out = Vec::new();
        let result = serve_with(session, &EffectiveConfig::from_env(), input, &mut out);
        let replies = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        (result, replies)
    }

    #[test]
    fn many_requests_are_served_by_one_session() {
        if local_shell().is_none() {
            return;
        }
        // `$$` is the remote shell's pid: one session means one pid.
        let (result, replies) = run(&[
            request("echo $PPID"),
            request("echo $PPID"),
            request("printf partial"),
        ]);
        result.unwrap();
        assert_eq!(replies.len(), 3);
        for reply in &replies {
            assert_eq!(reply["ok"], true);
            assert_eq!(reply["protocol_version"], 1);
            assert_eq!(reply["exit_code"], 0);
        }
        assert_eq!(replies[0]["stdout"], replies[1]["stdout"], "one session");
        assert_eq!(replies[2]["stdout"], "partial");
    }

    #[test]
    fn a_failing_command_is_an_ok_reply_with_its_exit_code() {
        if local_shell().is_none() {
            return;
        }
        let (result, replies) = run(&[request("echo out; echo err >&2; exit 3")]);
        result.unwrap();
        assert_eq!(replies[0]["ok"], true);
        assert_eq!(replies[0]["exit_code"], 3);
        let out = replies[0]["stdout"].as_str().unwrap();
        assert!(out.contains("out") && out.contains("err"), "{out}");
    }

    #[test]
    fn a_command_that_reads_stdin_cannot_swallow_the_next_request() {
        if local_shell().is_none() {
            return;
        }
        let (result, replies) = run(&[request("cat; echo after-cat"), request("echo second")]);
        result.unwrap();
        assert_eq!(replies[0]["stdout"], "after-cat");
        assert_eq!(replies[1]["stdout"], "second");
    }

    #[test]
    fn a_bad_request_is_an_error_reply_and_the_channel_lives_on() {
        if local_shell().is_none() {
            return;
        }
        let bad = json!({"cwd": "relative", "program": "git", "args": []}).to_string();
        let (result, replies) = run(&["not json".to_string(), bad, request("echo still-here")]);
        result.unwrap();
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[0]["ok"], false);
        assert_eq!(replies[1]["ok"], false);
        assert_eq!(replies[2]["stdout"], "still-here");
    }

    #[test]
    fn a_session_that_dies_ends_the_verb_without_a_reply() {
        let Some(mut dying) = local_shell() else {
            return;
        };
        dying.args(["-c", "exit 0"]);
        let input = Cursor::new(request("echo hi") + "\n");
        let mut out = Vec::new();
        let result = serve_with(dying, &EffectiveConfig::from_env(), input, &mut out);
        assert!(result.is_err(), "{result:?}");
        assert!(out.is_empty(), "a transport failure must not be answered");
    }
}
