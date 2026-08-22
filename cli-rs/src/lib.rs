//! `ralphus-cli` library: the `ralphus` CLI, ported from
//! `cli/src/ralphus/`. The binary (`src/main.rs`) wires this up to
//! `std::env::args`.

pub mod agents;
pub mod args;
pub mod client;
pub mod commands;
pub mod config;
pub mod entity_uri;
pub mod flags;
pub mod graphview;
pub mod health;
pub mod help_map;
pub mod output;
pub mod selector;
pub mod tutor;

/// Re-exported rather than duplicated -- see `runner/src/hostos.rs`; this
/// crate already depends on `ralphus-runner` for `shellcmd`/`llm_client`
/// reuse, so there is no reason for a third copy of one function.
pub use ralphus_runner::hostos;
