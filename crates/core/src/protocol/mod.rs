//! Local `brainprint-cli`/`brainprintd` wire contract (#15 task 9 / #13
//! task 3 §8).
//!
//! Started directly under `brainprint-core` rather than as a separate
//! workspace member: there is no independent protocol lifecycle yet (#13
//! task 3 §8), and both the daemon and future CLI/MCP clients already
//! depend on `brainprint-core`.
//!
//! - [`messages`]: the request/response data contract.
//! - [`framing`]: length-prefixed JSON framing, generic over any
//!   [`tokio::io::AsyncRead`]/`AsyncWrite` stream.
//! - [`transport`]: the local-only (never TCP) IPC stream itself -- Unix
//!   domain socket or Windows named pipe.

pub mod framing;
pub mod messages;
pub mod transport;

pub use messages::{
    ErrorResponse, HandshakeRequest, HandshakeResponse, Request, Response, StatusRequest,
    StatusResponse,
};
pub use transport::{ClientConnection, Listener, ServerConnection};
