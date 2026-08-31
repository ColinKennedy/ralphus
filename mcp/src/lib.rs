//! `ralphus-mcp`: an MCP server exposing ralphus's daemon HTTP API as MCP
//! tools (RAL-301). Talks to the daemon directly via `ralphus_cli`'s
//! `DaemonClient`, reused as a library -- no dependency on the compiled
//! `ralphus` binary. See `exec/mod.rs` for the execution architecture and
//! `tools.rs` for how the tool surface is derived from `help_map.rs`.

pub mod chip;
pub mod exclusions;
pub mod exec;
pub mod protocol;
pub mod server;
pub mod tools;
