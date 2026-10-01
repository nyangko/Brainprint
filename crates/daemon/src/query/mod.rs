//! Task 11 daemon query adapter (#24 "Implementation contract locked").
//!
//! [`convert_in`]/[`convert_out`] hold the explicit, bijective wire <->
//! Core conversions; [`runtime`] owns the per-Workspace worker threads,
//! `DeliveryLedger` lifetime and pending-acknowledgement state;
//! [`handler`] dispatches `Request::Query`/`Request::QueryAck` against it,
//! and `Request::Work` (#50) onto the same per-Workspace worker.

mod capture;
mod convert_in;
mod convert_out;
mod handler;
pub mod lifecycle;
mod observe;
mod runtime;
pub mod semantic;
#[cfg(test)]
mod verification;
mod verify;
mod work;

pub use handler::{handle_query, handle_query_ack, handle_work};
pub use runtime::DaemonQueryRuntime;
