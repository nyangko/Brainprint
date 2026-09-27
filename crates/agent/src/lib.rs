//! `brainprint-agent`: the I5 Task 13 Integration Gateway + thin signal
//! bridges (#26, #30). It routes project discovery toward Brainprint and
//! suppresses only proven duplicate work; any uncertainty preserves the
//! native fallback.
//!
//! Dependency boundary (#26 "Architecture"): `brainprint-core` plus
//! serde/serde_json/clap/tokio only. No Brainprint DB and no project
//! source is ever opened here; the only Brainprint access is the public
//! Task 11 local IPC, through [`probe`].

pub mod clients;
pub mod delivery;
pub mod event;
pub mod gateway;
pub mod probe;
pub mod shell;
pub mod state;
pub mod telemetry;
pub mod util;
