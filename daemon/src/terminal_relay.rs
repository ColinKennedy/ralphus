//! Remote Open Agent terminal relay over WebSocket (RAL-355 Phase 10).
//!
//! Gives a browser or CLI client an interactive terminal onto a **remote**
//! cell's Claude Code session -- something the existing `open-terminal?mode=agent`
//! path cannot do at all, since that spawns a *native terminal window on the
//! daemon's own desktop* (see `server.rs::open_agent_terminal`), which only
//! ever makes sense when the daemon and the human are on the same machine.
//! A browser client, or a remote machine's cell, has no way to see a window
//! popped open on a server's desktop.
//!
//! Local cells are untouched -- they keep using the existing native-terminal
//! mechanism; this relay refuses to handle them at all (see
//! `server.rs::mint_terminal_ticket_route`'s eligibility check).
//!
//! ## Why a second listener, not a route on the existing HTTP server
//!
//! `tiny_http` has no upgrade/hijack hook to hand a connection off to a
//! WebSocket library, so this binds its own dedicated port
//! (`daemon_port + 1` by default) instead. See `Cargo.toml`'s `tungstenite`
//! dependency comment for the fuller rationale (no async runtime in this
//! workspace, so this is the synchronous, `tokio`-free core crate).
//!
//! ## Session lifecycle: dies on disconnect, no reattach
//!
//! A closed WebSocket connection (browser tab closed, network drop, CLI
//! client exited) ends the remote session outright -- the spawned provider
//! `terminal` child (and the `ssh -tt` it in turn runs) is killed. This is a
//! deliberate v1 simplification: reattaching to a still-running remote
//! session would need durable session-handle bookkeeping this first version
//! doesn't build (see `REMOTE_IMPROVEMENTS.local.md`'s Phase 10 notes). To
//! resume, the human runs the existing headless Resume Automation, or opens
//! a fresh terminal relay connection, same as local Open Agent's own
//! resume path.
//!
//! ## Auth
//!
//! A browser's `WebSocket` constructor cannot attach a bearer header, so
//! this listener is gated by a short-lived, single-use ticket
//! (`crate::token::TicketStore`, same mechanism `/api/events` already uses
//! for the identical reason) minted via the ordinary bearer-authenticated
//! `POST .../terminal-ticket` route. The ticket only proves "an authorized
//! client asked for a terminal relay recently" -- the actual cell identity
//! and eligibility (remote, Claude Code, resumable) are re-resolved fresh
//! from the WS connection's own query parameters at connect time, not
//! trusted from ticket-mint time, so a cell that changed state in between
//! is still refused correctly.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tungstenite::{Message, WebSocket};

use crate::cartographer::Note;
use crate::server::{Daemon, query_param};

/// How often the relay loop polls for new output from the remote child
/// process while also listening for WebSocket frames -- small enough that
/// typing feels immediate, large enough not to busy-loop.
const POLL_INTERVAL: Duration = Duration::from_millis(30);
const CHILD_READ_BUFFER: usize = 4096;

/// Bind the terminal-relay listener on `(bind_ip, port)` (`port` `0` picks
/// an OS-assigned ephemeral port -- used by tests), spawn its accept loop on
/// its own thread, record the bound port on `daemon` (so ticket-mint
/// responses can tell a client where to connect), and return the bound port.
///
/// `bind_ip` is passed in rather than hardcoded to loopback so the relay
/// listens on whatever host the main HTTP server itself bound to (`serve()`
/// passes the same IP it just bound `tiny_http::Server` on) -- a remote
/// browser client needs to reach this port exactly as it reaches the daemon
/// API, not just a client on the daemon's own machine.
///
/// # Errors
/// The port failing to bind (already in use, insufficient permissions).
pub fn start(bind_ip: IpAddr, port: u16, daemon: Arc<Daemon>) -> std::io::Result<u16> {
    let listener = TcpListener::bind((bind_ip, port))?;
    let bound_port = listener.local_addr()?.port();
    daemon.set_terminal_relay_port(bound_port);
    crate::rlog!(
        INFO,
        "ralphus [terminal] relay listening on {bind_ip}:{bound_port}"
    );
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let daemon = Arc::clone(&daemon);
            thread::spawn(move || handle_connection(stream, &daemon));
        }
    });
    Ok(bound_port)
}

fn handle_connection(stream: TcpStream, daemon: &Arc<Daemon>) {
    let mut captured_query: Option<String> = None;
    // `tungstenite`'s own `ErrorResponse` (`http::Response<Option<Vec<u8>>>`)
    // is large enough to trip `clippy::result_large_err` -- not something a
    // caller of this library-defined callback signature can shrink.
    #[allow(clippy::result_large_err)]
    let handshake = tungstenite::accept_hdr(
        stream,
        |req: &tungstenite::handshake::server::Request, response| {
            captured_query = Some(req.uri().query().unwrap_or("").to_string());
            Ok(response)
        },
    );
    let mut ws = match handshake {
        Ok(ws) => ws,
        Err(e) => {
            crate::rlog!(WARNING, "ralphus [terminal] handshake failed: {e}");
            return;
        }
    };
    let query = captured_query.unwrap_or_default();
    if let Err(reason) = run_session(&mut ws, &query, daemon) {
        crate::rlog!(WARNING, "ralphus [terminal] session refused: {reason}");
        let _ = ws.send(Message::Text(format!("ralphus: {reason}\r\n")));
    }
    let _ = ws.close(None);
}

/// RAII release for [`Daemon::try_acquire_terminal_session`] -- guarantees
/// the slot frees on every exit path out of [`run_session`] (early error
/// return, a clean session end, or a panic unwinding through here), not
/// just the happy path.
struct SessionSlot<'a> {
    daemon: &'a Arc<Daemon>,
    squad_id: String,
    task_idx: i64,
    cell_idx: i64,
}

impl Drop for SessionSlot<'_> {
    fn drop(&mut self) {
        self.daemon
            .release_terminal_session(&self.squad_id, self.task_idx, self.cell_idx);
    }
}

struct SessionParams {
    squad_id: String,
    task_idx: i64,
    cell_idx: i64,
    cols: u16,
    lines: u16,
}

fn parse_params(query: &str) -> Result<SessionParams, String> {
    let squad_id = query_param(query, "squad_id")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing squad_id".to_string())?
        .to_string();
    let task_idx = query_param(query, "task_idx")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "missing or invalid task_idx".to_string())?;
    let cell_idx = query_param(query, "cell_idx")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "missing or invalid cell_idx".to_string())?;
    let cols = query_param(query, "cols")
        .and_then(|s| s.parse().ok())
        .unwrap_or(80);
    let lines = query_param(query, "lines")
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);
    Ok(SessionParams {
        squad_id,
        task_idx,
        cell_idx,
        cols,
        lines,
    })
}

fn run_session(
    ws: &mut WebSocket<TcpStream>,
    query: &str,
    daemon: &Arc<Daemon>,
) -> Result<(), String> {
    if !daemon.consume_terminal_ticket(query_param(query, "ticket")) {
        return Err("invalid, expired, or already-used ticket".to_string());
    }
    let params = parse_params(query)?;

    // Re-resolve eligibility fresh at connect time -- cell state may have
    // changed since the ticket was minted (see the module doc comment).
    let (cwd, agent, agent_session_id, machine, state) = {
        let guard = daemon.lock();
        let (cwd, agent, agent_session_id) = guard
            .get_cell_agent_resume(&params.squad_id, params.task_idx, params.cell_idx)
            .map_err(|e| e.to_string())?;
        let (machine, _is_command_cell, state) = guard
            .get_cell_input_gate(&params.squad_id, params.task_idx, params.cell_idx)
            .map_err(|e| e.to_string())?;
        (cwd, agent, agent_session_id, machine, state)
    };
    let Some(machine) = machine else {
        return Err("this cell runs locally, not remotely".to_string());
    };
    if state == crate::store::NodeState::Running {
        return Err(
            "this cell is still running remotely -- wait for it to finish before opening a \
             terminal"
                .to_string(),
        );
    }
    if !ralphus_core::agent_resume::is_claude_agent(Some(agent.as_str())) {
        return Err(format!(
            "the remote terminal relay only supports Claude Code cells; this cell's agent is {agent:?}"
        ));
    }
    let Some(session_id) = agent_session_id else {
        return Err("no resumable agent session recorded yet for this cell".to_string());
    };

    // One attached session per cell at a time -- two independent WS clients
    // both resuming `agent_session_id` concurrently would race the same
    // conversation transcript on the remote machine (see the field's own
    // doc comment on `Daemon::active_terminal_sessions`).
    if !daemon.try_acquire_terminal_session(&params.squad_id, params.task_idx, params.cell_idx) {
        return Err(
            "a terminal session is already attached to this cell -- only one at a time is \
             allowed"
                .to_string(),
        );
    }
    let _session_slot = SessionSlot {
        daemon,
        squad_id: params.squad_id.clone(),
        task_idx: params.task_idx,
        cell_idx: params.cell_idx,
    };

    let provider = {
        let guard = daemon.lock();
        crate::remote_runner::provider_from_store(&guard, &machine).map_err(|e| e.to_string())?
    };
    let Some(provider) = provider else {
        return Err(format!("machine {machine:?} resolved to the local host"));
    };

    let claude_program =
        std::env::var("RALPHUS_CLAUDE_COMMAND").unwrap_or_else(|_| "claude".to_string());
    let command =
        ralphus_core::agent_resume::resume_agent_command_posix(&claude_program, &session_id);

    let session_name = crate::tmux::session_name(
        &params.squad_id,
        &format!("t{}-c{}", params.task_idx, params.cell_idx),
        "remote-terminal",
    );
    let attempt = crate::terminal_log::list_attempts(&session_name).len() as u32;
    let log_path = crate::terminal_log::attempt_path(&session_name, attempt);
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    log_open_event(
        daemon,
        &params,
        &machine,
        &cwd,
        log_path.to_string_lossy().as_ref(),
    );

    let mut child = provider.spawn_terminal(&command, params.cols, params.lines)?;
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| "provider child had no stdin".to_string())?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| "provider child had no stdout".to_string())?;

    // Drain the child's stdout on its own thread into a channel, so the main
    // loop below can poll it without blocking on a `read()` that might never
    // return if the remote side goes quiet -- interleaved with polling the
    // WebSocket for incoming keystrokes on the same thread (tungstenite's
    // `WebSocket` is not meant to be read/written from two threads at once).
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader = thread::spawn(move || {
        let mut stdout = child_stdout;
        let mut buf = [0_u8; CHILD_READ_BUFFER];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    if let Err(e) = ws.get_ref().set_read_timeout(Some(POLL_INTERVAL)) {
        crate::rlog!(
            WARNING,
            "ralphus [terminal] could not set a read timeout, falling back to blocking reads: {e}"
        );
    }

    let outcome = relay_loop(ws, &mut child, &mut child_stdin, &rx, log_file.as_mut());

    drop(child_stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    log_close_event(daemon, &params, outcome.as_ref().err().map(String::as_str));

    outcome
}

/// The core byte-relay loop: WebSocket frames in, remote pty bytes out,
/// until the client closes, the remote process exits, or an unrecoverable
/// I/O error occurs. Runs entirely on the calling thread.
fn relay_loop(
    ws: &mut WebSocket<TcpStream>,
    child: &mut std::process::Child,
    child_stdin: &mut std::process::ChildStdin,
    rx: &mpsc::Receiver<Vec<u8>>,
    mut log_file: Option<&mut std::fs::File>,
) -> Result<(), String> {
    loop {
        // Forward any buffered remote output before checking for new input,
        // so a burst of output is never held back behind a slow client.
        while let Ok(chunk) = rx.try_recv() {
            if let Some(f) = log_file.as_deref_mut() {
                let _ = f.write_all(&chunk);
            }
            if ws.send(Message::Binary(chunk)).is_err() {
                return Ok(());
            }
        }

        if let Ok(Some(_status)) = child.try_wait() {
            return Ok(());
        }

        match ws.read() {
            Ok(Message::Binary(data)) => {
                if child_stdin.write_all(&data).is_err() {
                    return Ok(());
                }
            }
            Ok(Message::Text(data)) => {
                if child_stdin.write_all(data.as_bytes()).is_err() {
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
                // Loop back around to check for child output/exit again.
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(format!("websocket error: {e}")),
        }
    }
}

fn log_open_event(
    daemon: &Arc<Daemon>,
    params: &SessionParams,
    machine: &str,
    cwd: &str,
    log_path: &str,
) {
    let guard = daemon.lock();
    let cell_id = format!("{}/{}", params.task_idx, params.cell_idx);
    Note::new("terminal")
        .scope("cell")
        .squad(&params.squad_id)
        .cell(&cell_id)
        .log_path(log_path)
        .emit(
            &guard,
            "remote terminal session opened",
            serde_json::json!({
                "task_idx": params.task_idx,
                "cell_idx": params.cell_idx,
                "machine": machine,
                "cwd": cwd,
            }),
        );
}

fn log_close_event(daemon: &Arc<Daemon>, params: &SessionParams, error: Option<&str>) {
    let guard = daemon.lock();
    let cell_id = format!("{}/{}", params.task_idx, params.cell_idx);
    Note::new("terminal")
        .level(if error.is_some() {
            crate::logging::LogLevel::WARNING
        } else {
            crate::logging::LogLevel::INFO
        })
        .scope("cell")
        .squad(&params.squad_id)
        .cell(&cell_id)
        .emit(
            &guard,
            "remote terminal session closed",
            serde_json::json!({
                "task_idx": params.task_idx,
                "cell_idx": params.cell_idx,
                "error": error,
            }),
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_params_reads_every_field() {
        let p = parse_params("ticket=abc&squad_id=squad-1&task_idx=0&cell_idx=2&cols=100&lines=40")
            .unwrap();
        assert_eq!(p.squad_id, "squad-1");
        assert_eq!(p.task_idx, 0);
        assert_eq!(p.cell_idx, 2);
        assert_eq!(p.cols, 100);
        assert_eq!(p.lines, 40);
    }

    #[test]
    fn parse_params_defaults_cols_and_lines() {
        let p = parse_params("squad_id=squad-1&task_idx=0&cell_idx=0").unwrap();
        assert_eq!(p.cols, 80);
        assert_eq!(p.lines, 24);
    }

    #[test]
    fn parse_params_requires_squad_id() {
        assert!(parse_params("task_idx=0&cell_idx=0").is_err());
    }

    #[test]
    fn parse_params_requires_valid_task_and_cell_index() {
        assert!(parse_params("squad_id=s&task_idx=nope&cell_idx=0").is_err());
        assert!(parse_params("squad_id=s&task_idx=0&cell_idx=nope").is_err());
    }
}
