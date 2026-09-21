//! `brainprintd`: the Brainprint global daemon (#15 task 9 / #13 task 4).
//!
//! One instance per user, never per Workspace. Hosts `brainprint-engine`
//! behind a local-only (never network) IPC boundary; does not re-implement
//! DB/domain logic itself.
//!
//! This crate currently implements only the minimal handshake + status
//! path (#15 task 9). Watcher, semantic backend, MCP, and Command
//! Intelligence are out of scope here, and CLI subcommand wiring
//! (`brainprint install/init/status`) is #15 task 10.

pub mod client;
pub mod runtime_paths;
pub mod server;
pub mod state;
