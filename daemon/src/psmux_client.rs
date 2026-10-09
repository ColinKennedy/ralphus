//! Talking to a psmux server directly, instead of spawning a `tmux.exe`
//! client process for every command.
//!
//! On Windows the multiplexer is psmux, which runs one server process per
//! session. Its `tmux.exe` CLI is only a messenger: it reads the session's
//! `<data dir>/<name>.port` and `<name>.key` files, opens a loopback TCP
//! connection, sends `AUTH <key>`, a `TARGET <name>` line and one command
//! line, half-closes, and reads the reply to EOF (see vendored psmux
//! `src/session.rs`'s `send_control` / `send_control_with_response`). Every
//! ralphus `has-session`, `capture-pane`, `set-option`, `pipe-pane`,
//! `send-keys`, `clear-history` and `kill-session` paid a process creation
//! (plus a conhost from a console-less daemon) for that. This module does the
//! same exchange in-process: reading two small files and one socket, no
//! process.
//!
//! It mirrors the CLI's wire format exactly, including the trailing
//! `session-info` barrier fire-and-forget commands send so the reply proves
//! the command executed, and psmux's argument quoting (`quote_arg_if_needed`).
//! Callers in [`crate::tmux`] fall back to the CLI whenever this returns
//! [`Error::Unavailable`], and [`disable`] turns it off for the rest of the
//! process if the data directory it computes ever disagrees with the one the
//! CLI used. `new-session` is never sent this way: it starts the server, and
//! that process must be spawned so it lands in the cell's Job Object.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Matches the CLI's connect timeout: long enough that a busy server is not
/// mistaken for a dead one.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1000);
/// Matches the CLI's reply timeout for commands that return content.
const READ_TIMEOUT: Duration = Duration::from_millis(3000);
/// Cap on one reply, far above any capture ralphus asks for.
const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;

/// Set once the in-process path has proven unreliable; every later call goes
/// straight to the CLI.
static DISABLED: AtomicBool = AtomicBool::new(false);

/// Why a request could not be answered in-process.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// No server is registered under this session name (no `.port` file in
    /// the data directory) -- the same condition the CLI reports as "no server
    /// running on session".
    NoServer,
    /// The server answered with an error line.
    Refused(String),
    /// The in-process path cannot be used here (disabled, not Windows, no
    /// data directory, connection or timeout trouble); the caller should run
    /// the CLI instead.
    Unavailable(String),
}

/// Whether this process may use the in-process client at all.
#[must_use]
pub fn enabled() -> bool {
    cfg!(windows)
        && !DISABLED.load(Ordering::Relaxed)
        && std::env::var("RALPHUS_TMUX_NATIVE_CLIENT").map_or(true, |v| v != "0")
        && data_dir().is_some_and(|d| d.is_dir())
}

/// Stop using the in-process client for the rest of this process.
pub fn disable(reason: &str) {
    if !DISABLED.swap(true, Ordering::Relaxed) {
        // ralphus[ignore-rlog-pair]: low-level transport switch with no Store; the CLI fallback keeps every caller's behavior unchanged
        crate::rlog!(
            WARNING,
            "ralphus [tmux] in-process psmux client disabled, using the tmux CLI instead: {reason}"
        );
    }
}

/// psmux's data directory: `PSMUX_DATA_DIR` when set to an absolute path,
/// otherwise `<user profile>\.psmux` (psmux's `paths::psmux_dir_opt`).
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("PSMUX_DATA_DIR") {
        let path = PathBuf::from(raw);
        return path.is_absolute().then_some(path);
    }
    std::env::var_os("USERPROFILE")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".psmux"))
}

/// Whether the server for `name` has written its port file where this module
/// looks -- used to check that our data directory matches the CLI's.
#[must_use]
pub fn port_file_exists(name: &str) -> bool {
    data_dir().is_some_and(|d| d.join(format!("{name}.port")).is_file())
}

fn endpoint(dir: &Path, name: &str) -> Result<(SocketAddr, String), Error> {
    let port = std::fs::read_to_string(dir.join(format!("{name}.port")))
        .map_err(|_| Error::NoServer)?
        .trim()
        .parse::<u16>()
        .map_err(|_| Error::Unavailable(format!("unreadable port file for {name}")))?;
    let key = std::fs::read_to_string(dir.join(format!("{name}.key")))
        .map(|k| k.trim().to_string())
        .unwrap_or_default();
    if key.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(Error::Unavailable(format!("malformed key file for {name}")));
    }
    Ok((SocketAddr::from(([127, 0, 0, 1], port)), key))
}

/// Send one command line to `name`'s server and return its reply, with the
/// leading `OK` AUTH acknowledgement removed. `barrier` appends the
/// `session-info` round-trip the CLI uses for fire-and-forget commands, so
/// the call returns only once the command has executed.
fn request(name: &str, line: &str, barrier: bool) -> Result<String, Error> {
    if !enabled() {
        return Err(Error::Unavailable("disabled".into()));
    }
    let dir = data_dir().ok_or_else(|| Error::Unavailable("no psmux data directory".into()))?;
    request_in(&dir, name, line, barrier)
}

/// [`request`] against the server registered in `dir`.
fn request_in(dir: &Path, name: &str, line: &str, barrier: bool) -> Result<String, Error> {
    let (addr, key) = endpoint(dir, name)?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| {
        if e.kind() == std::io::ErrorKind::ConnectionRefused {
            Error::NoServer
        } else {
            Error::Unavailable(format!("connect to {name}: {e}"))
        }
    })?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| Error::Unavailable(e.to_string()))?;
    let mut message = format!("AUTH {key}\nTARGET {name}\n{line}");
    if !message.ends_with('\n') {
        message.push('\n');
    }
    if barrier {
        message.push_str("session-info\n");
    }
    stream
        .write_all(message.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| Error::Unavailable(format!("write to {name}: {e}")))?;
    let _ = stream.shutdown(Shutdown::Write);
    let mut reply = Vec::new();
    if let Err(e) = (&mut stream).take(MAX_REPLY_BYTES).read_to_end(&mut reply) {
        return Err(Error::Unavailable(format!("read from {name}: {e}")));
    }
    let reply = String::from_utf8_lossy(&reply).into_owned();
    let body = reply
        .strip_prefix("OK\r\n")
        .or_else(|| reply.strip_prefix("OK\n"))
        .unwrap_or(&reply);
    let trimmed = body.trim();
    if trimmed == "ERROR: Authentication required" || trimmed == "ERROR: Invalid session key" {
        return Err(Error::Unavailable(format!("{name}: {trimmed}")));
    }
    Ok(body.to_string())
}

/// psmux's `quote_arg_if_needed`: quote a value only when the server's
/// tokenizer would otherwise split or strip it, escaping backslash, `"`, and
/// the line terminators so the argument stays one token on one wire line.
#[must_use]
pub fn quote_arg_if_needed(s: &str) -> String {
    if s.is_empty() || s.chars().any(char::is_whitespace) || s.contains('"') || s.contains('\'') {
        let escaped = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// Whether `name`'s server is alive and authenticates. `None` when this can't
/// be answered in-process.
#[must_use]
pub fn has_session(name: &str) -> Option<bool> {
    match request(name, "session-info", false) {
        Ok(_) => Some(true),
        Err(Error::NoServer) => Some(false),
        Err(Error::Refused(_)) => Some(false),
        Err(Error::Unavailable(_)) => None,
    }
}

/// `capture-pane -p -S -<lines>` for `name`'s active pane.
///
/// # Errors
/// [`Error::NoServer`] when the session is gone; [`Error::Unavailable`] when
/// the caller should use the CLI.
pub fn capture_pane(name: &str, lines: u32) -> Result<String, Error> {
    request(name, &format!("capture-pane -p -S -{lines}"), false)
}

/// A command that returns text (`show-options -v <name>`), reply as sent.
///
/// # Errors
/// As [`capture_pane`].
pub fn query(name: &str, line: &str) -> Result<String, Error> {
    request(name, line, false)
}

/// A fire-and-forget command (`set-option`, `send-keys`, `clear-history`,
/// `kill-session`), confirmed executed by the barrier.
///
/// # Errors
/// As [`capture_pane`].
pub fn command(name: &str, line: &str) -> Result<(), Error> {
    request(name, line, true).map(|_| ())
}

/// A command whose acceptance is silent and whose refusal is an `ERROR`
/// reply (`pipe-pane`).
///
/// # Errors
/// [`Error::Refused`] with the server's message on refusal; otherwise as
/// [`capture_pane`].
pub fn checked_command(name: &str, line: &str) -> Result<(), Error> {
    let reply = request(name, line, false)?;
    if reply.trim_start().starts_with("ERROR") {
        Err(Error::Refused(reply.trim().to_string()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead as _;

    #[test]
    fn quoting_matches_psmux() {
        assert_eq!(quote_arg_if_needed("Enter"), "Enter");
        assert_eq!(quote_arg_if_needed(r"C:\a\b"), r"C:\a\b");
        assert_eq!(quote_arg_if_needed(""), "\"\"");
        assert_eq!(quote_arg_if_needed("a b"), "\"a b\"");
        assert_eq!(quote_arg_if_needed(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg_if_needed("C:\\x y\\"), "\"C:\\\\x y\\\\\"");
        assert_eq!(quote_arg_if_needed("a\nb c"), "\"a\\nb c\"");
    }

    /// A fake psmux server: records the request lines and replies like the
    /// real one (`OK` ack, then `body`).
    fn fake_server(body: &'static str) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut lines = Vec::new();
            let mut line = String::new();
            while reader.read_line(&mut line).expect("read") > 0 {
                lines.push(line.trim_end().to_string());
                line.clear();
            }
            let mut stream = stream;
            stream.write_all(b"OK\n").expect("ack");
            stream.write_all(body.as_bytes()).expect("body");
            lines
        });
        (port, handle)
    }

    #[test]
    fn requests_use_the_cli_wire_format_and_strip_the_ack() {
        let dir = std::env::temp_dir().join(format!("ralphus-psmux-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let (port, server) = fake_server(
            "line one
line two
",
        );
        std::fs::write(dir.join("sess.port"), port.to_string()).expect("port");
        std::fs::write(
            dir.join("sess.key"),
            "secret
",
        )
        .expect("key");

        let out = request_in(&dir, "sess", "capture-pane -p -S -40", false).expect("capture");
        assert_eq!(
            out,
            "line one
line two
"
        );
        assert_eq!(
            server.join().expect("server"),
            vec!["AUTH secret", "TARGET sess", "capture-pane -p -S -40"]
        );

        let (port, server) = fake_server("");
        std::fs::write(dir.join("sess.port"), port.to_string()).expect("port");
        request_in(&dir, "sess", "set-option remain-on-exit on", true).expect("set-option");
        assert_eq!(
            server.join().expect("server"),
            vec![
                "AUTH secret",
                "TARGET sess",
                "set-option remain-on-exit on",
                "session-info"
            ]
        );

        assert_eq!(
            request_in(&dir, "missing", "capture-pane -p -S -5", false),
            Err(Error::NoServer)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
