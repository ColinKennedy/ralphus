//! CLI client for the remote Open Agent terminal relay (RAL-355 Phase 10) --
//! backs `ralphus cell remote-terminal <selector>`.
//!
//! Unlike every other command in this crate, this one is not a single
//! HTTP/JSON round trip: it mints a ticket over the ordinary
//! [`DaemonClient`], then opens its own raw WebSocket connection (this
//! crate's only WS use -- everything else in `client.rs` is plain
//! `ureq`/HTTP) and relays this process's own stdin/stdout to it
//! byte-for-byte until the remote session ends. This is the terminal
//! twin of `board.html`'s vendored terminal-emulator client and the
//! daemon's own `terminal` provider verb -- all three speak the identical
//! raw-byte-over-WebSocket contract described in
//! `docs/machine-providers.md`'s "The `terminal` verb" section.
//!
//! Local cells are unaffected -- they keep using `ralphus cell terminal`
//! (which just prints the resume command for a human to run themselves) and
//! `ralphus cell open-agent`. This command exists because neither of those
//! works for a cell whose agent conversation lives on a **remote** machine:
//! there is no local command to print, and nothing on this machine to open a
//! window onto.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tungstenite::{Message, WebSocket};

use crate::client::DaemonClient;

const POLL_INTERVAL: Duration = Duration::from_millis(30);
const STDIN_READ_BUFFER: usize = 4096;

/// Enables raw terminal mode for the lifetime of the guard, restoring the
/// terminal's normal (cooked) mode on drop -- including on an early return
/// or panic, so a crash mid-session never leaves the user's shell stuck in
/// raw mode with no visible echo.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self, String> {
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| format!("could not put this terminal into raw mode: {e}"))?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Mint a ticket for `squad_id`/`task_idx`/`cell_idx`, connect the relay's
/// WebSocket, and drive an interactive terminal on this process's own
/// stdin/stdout until the session ends -- the remote process exits, the
/// connection drops, or this terminal disconnects (Ctrl+D/Ctrl+C reach the
/// remote side as raw bytes, the same as any other pty-backed SSH session;
/// there is no local-side session end shortcut).
///
/// # Errors
/// Ticket-mint failure (see `docs/daemon-api.md`'s `terminal-ticket` route
/// for the possible `400`/`409`/`503` reasons), a WebSocket connect/handshake
/// failure, or this process's stdin not being a real terminal (raw mode
/// requires one).
pub fn run(
    client: &DaemonClient,
    squad_id: &str,
    task_idx: i64,
    cell_idx: i64,
) -> Result<(), String> {
    let resp = client
        .mint_terminal_ticket(squad_id, task_idx, cell_idx)
        .map_err(|e| e.to_string())?;
    let ticket = resp["ticket"]
        .as_str()
        .ok_or("daemon did not return a ticket")?
        .to_string();
    let port = resp["port"]
        .as_u64()
        .ok_or("daemon did not return a relay port")? as u16;
    let path = resp["path"].as_str().unwrap_or("/terminal");

    let (cols, lines) = crossterm::terminal::size().unwrap_or((80, 24));
    let host = ws_host(client.base_url());
    let url = format!(
        "ws://{host}:{port}{path}?ticket={ticket}&squad_id={squad_id}&task_idx={task_idx}&cell_idx={cell_idx}&cols={cols}&lines={lines}"
    );

    let tcp = TcpStream::connect((host.as_str(), port))
        .map_err(|e| format!("could not reach the terminal relay at {host}:{port}: {e}"))?;
    let (mut ws, _handshake_response) = tungstenite::client(&url, tcp)
        .map_err(|e| format!("terminal relay handshake failed: {e}"))?;

    // Raw mode only from here on -- everything above this point is ordinary
    // HTTP/connect diagnostics a human should see printed normally.
    let _raw_mode = RawModeGuard::enable()?;

    if let Err(e) = ws.get_ref().set_read_timeout(Some(POLL_INTERVAL)) {
        eprintln!(
            "ralphus: could not set a read timeout on the terminal relay connection, falling back to blocking reads: {e}\r"
        );
    }

    // Drains this process's own stdin on its own thread into a channel, the
    // same pattern the daemon side uses for the remote child's stdout --
    // `ws.read()`'s timeout is what keeps the loop below from blocking
    // forever on either side going quiet.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0_u8; STDIN_READ_BUFFER];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        // This thread outlives the relay loop below whenever the *remote*
        // side ends the session first (stdin.read() blocks until the next
        // keystroke) -- harmless, since it exits the moment this process
        // does, and there is nothing left for it to relay to by then.
    });

    relay_loop(&mut ws, &rx)
}

fn relay_loop(ws: &mut WebSocket<TcpStream>, rx: &mpsc::Receiver<Vec<u8>>) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    loop {
        while let Ok(chunk) = rx.try_recv() {
            if ws.send(Message::Binary(chunk)).is_err() {
                return Ok(());
            }
        }
        match ws.read() {
            Ok(Message::Binary(data)) => {
                if stdout.write_all(&data).is_err() || stdout.flush().is_err() {
                    return Ok(());
                }
            }
            Ok(Message::Text(data)) => {
                if stdout.write_all(data.as_bytes()).is_err() || stdout.flush().is_err() {
                    return Ok(());
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(ref io_err))
                if io_err.kind() == std::io::ErrorKind::WouldBlock
                    || io_err.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Expected: the read-timeout elapsed with nothing to read.
                // Loop back around to check for buffered stdin input too.
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(format!("terminal relay connection error: {e}")),
        }
    }
}

/// Extracts just the host from a daemon base URL (`"http://127.0.0.1:7890"`
/// -> `"127.0.0.1"`) -- the relay listens on the same host as the daemon
/// API itself, one port above it, so the ticket response's own `port` is
/// combined with this host rather than a second URL from the daemon.
fn ws_host(base_url: &str) -> String {
    let without_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    host_port.split(':').next().unwrap_or(host_port).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_host_strips_scheme_port_and_path() {
        assert_eq!(ws_host("http://127.0.0.1:7890"), "127.0.0.1");
        assert_eq!(
            ws_host("https://ralphus.example.com:8443/"),
            "ralphus.example.com"
        );
    }

    #[test]
    fn ws_host_handles_a_bare_host_with_no_scheme() {
        assert_eq!(ws_host("localhost:7890"), "localhost");
    }
}
