//! `brainprintd`: the Brainprint global daemon (#15 task 9/10 / #13 task
//! 4).
//!
//! One instance per user, never per Workspace. Hosts `brainprint-engine`
//! behind a local-only (never network) IPC boundary; does not re-implement
//! DB/domain logic itself -- [`handlers`] only calls straight into
//! existing engine APIs and maps their errors onto the wire contract.
//!
//! Handshake/status (#15 task 9) and install/init (#15 task 10) are
//! implemented. Watcher, semantic backend, MCP, Command Intelligence,
//! doctor/rebuild/uninit are out of scope here.

pub mod client;
pub mod handlers;
pub mod runtime_paths;
pub mod server;
pub mod state;
