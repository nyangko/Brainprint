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
//! - [`endpoint`]: where that stream lives -- the one path derivation both
//!   `brainprint-daemon` and `brainprint-cli` share (#15 task 10).

pub mod endpoint;
pub mod framing;
pub mod messages;
pub mod query;
pub mod transport;

pub use endpoint::{EndpointPaths, EndpointResolutionError};
pub use messages::{
    ErrorKind, ErrorResponse, HandshakeRequest, HandshakeResponse, InitRequest, InitResponse,
    InstallRequest, InstallResponse, Request, Response, StatusRequest, StatusResponse,
};
pub use transport::{ClientConnection, Listener, ServerConnection};
