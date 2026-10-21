//! `ralphus-fleet`: the built-in machine provider service (provider contract
//! v2, `docs/machine-provider-service.md`) and its reference implementation.
//!
//! Ralphus asks it for a machine that satisfies a request; it picks one of
//! the machines in its own configuration, hands back an allocation, and
//! serves that allocation's Machine API by driving the machine over SSH
//! through `ralphus-ssh-provider`. Which machines exist and how they are
//! reached is entirely this service's business.

pub mod allocations;
pub mod config;
pub mod machine_api;
pub mod server;
