//! Task 11 daemon query adapter (#24 "Implementation contract locked").
//!
//! [`convert_in`]/[`convert_out`] hold the explicit, bijective wire <->
//! Core conversions; [`runtime`] owns the per-Workspace worker threads,
//! `DeliveryLedger` lifetime and pending-acknowledgement state;
//! [`handler`] dispatches `Request::Query`/`Request::QueryAck` against it.

mod convert_in;
mod convert_out;
mod handler;
mod runtime;

pub use handler::{handle_query, handle_query_ack};
pub use runtime::DaemonQueryRuntime;
