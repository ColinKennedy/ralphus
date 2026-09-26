//! `ralphus-mcp` binary entry point: parses `--daemon-url`/`--read-only`,
//! then runs the MCP stdio loop.
//!
//! MCP clients (Claude Code, Codex) spawn this binary detached, with only
//! its stdio pipes connected and no console. On Windows that makes a
//! console-subsystem exe pop a new, visible console window for the
//! process's lifetime. The `windows` subsystem below stops that
//! allocation; the process still reads/writes fine over its inherited
//! stdio pipes (the same mechanism `pythonw.exe` relies on), and running
//! it manually from an existing terminal still prints into that terminal.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::{BufReader, stdin, stdout};

use ralphus_cli::client::DaemonClient;
use ralphus_mcp::protocol;
use ralphus_mcp::server::Server;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut daemon_url = std::env::var("RALPHUS_DAEMON_URL")
        .unwrap_or_else(|_| DaemonClient::default_url().to_string());
    let mut read_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--read-only" => read_only = true,
            "--daemon-url" => {
                if let Some(v) = args.get(i + 1) {
                    daemon_url = v.clone();
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let server = Server::new(daemon_url, read_only);
    if let Err(e) = protocol::serve(&server, BufReader::new(stdin()), stdout()) {
        eprintln!("ralphus-mcp: fatal I/O error: {e}");
        std::process::exit(1);
    }
}
