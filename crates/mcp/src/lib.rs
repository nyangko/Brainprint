//! `brainprint-mcp`: a thin stdio MCP adapter over the existing Task 11
//! local IPC (#25 "Purpose"). `main.rs` is a thin binary wrapper over
//! this library; the library split exists so integration tests (in
//! particular `brainprint-daemon`'s populated-Workspace fixture, which
//! `brainprint-mcp` itself must never depend on -- #25 "Crate boundary")
//! can drive [`server::BrainprintMcp`] directly, in-process, against a
//! real daemon.

pub mod daemon;
pub mod envelope;
pub mod execute;
pub mod params;
pub mod server;
pub mod tools;

pub use server::BrainprintMcp;
