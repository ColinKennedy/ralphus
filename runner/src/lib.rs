//! `ralphus-runner` library: executes one [`spec::SessionSpec`] and produces a
//! [`spec::SessionResult`]. Ported from `cli/src/ralphus/runner/`. The binary
//! (`src/main.rs`) is a thin shell around [`execute::run_session`] that reads
//! `SessionSpec` JSON from stdin and writes `SessionResult` JSON to stdout --
//! the daemon<->runner wire contract is unchanged from the Python runner.

pub mod agent_backend;
pub mod backend;
pub mod cartographer;
pub mod claude_code_backend;
pub mod cli_agent_common;
pub mod codex_backend;
pub mod config;
pub mod execute;
pub mod harness_backend;
pub mod hostos;
pub mod llm_client;
pub mod otel;
pub mod providers;
pub mod shellcmd;
pub mod spec;
pub mod tools;
