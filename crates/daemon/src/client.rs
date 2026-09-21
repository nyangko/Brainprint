//! Reusable client-side connect + handshake + status helpers (#15 task 9).
//!
//! CLI subcommand wiring (`brainprint status`, auto-start, ...) is #15
//! task 10 and deliberately not done here -- but the client *half* of the
//! handshake/status path has to be real and independently testable now,
//! not just the server's accept loop. This is also what the daemon's own
//! startup-time liveness probe (see [`crate::server`]) reuses to tell a
//! live endpoint from a stale one.

use std::{error::Error, fmt, io};

use brainprint_core::{
    PROTOCOL_VERSION,
    protocol::{
        self, ClientConnection, ErrorResponse, HandshakeRequest, HandshakeResponse, Request,
        Response, StatusRequest, StatusResponse,
    },
};

use crate::runtime_paths::RuntimeEndpoint;

/// Failure connecting to, handshaking with, or querying the daemon.
#[derive(Debug)]
pub enum ClientError {
    /// No daemon is listening at the resolved endpoint (connection
    /// refused / endpoint not found) -- distinct from an unexpected I/O
    /// failure so callers can tell "not running" from "broken".
    DaemonNotRunning(io::Error),
    Io(io::Error),
    /// The daemon rejected this client's protocol version.
    VersionMismatch {
        server_protocol_version: u32,
        client_protocol_version: u32,
    },
    /// The daemon sent a response shape that does not match the request.
    UnexpectedResponse,
    /// The daemon returned an explicit error for this request.
    Rejected(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DaemonNotRunning(source) => {
                write!(formatter, "no daemon is running at this endpoint: {source}")
            }
            Self::Io(source) => write!(formatter, "daemon client I/O error: {source}"),
            Self::VersionMismatch {
                server_protocol_version,
                client_protocol_version,
            } => write!(
                formatter,
                "protocol version mismatch: daemon speaks {server_protocol_version}, this client speaks {client_protocol_version}"
            ),
            Self::UnexpectedResponse => {
                formatter.write_str("daemon sent a response that did not match the request")
            }
            Self::Rejected(message) => write!(formatter, "daemon rejected the request: {message}"),
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DaemonNotRunning(source) | Self::Io(source) => Some(source),
            Self::VersionMismatch { .. } | Self::UnexpectedResponse | Self::Rejected(_) => None,
        }
    }
}

/// Connect to the resolved daemon endpoint. Does not handshake.
pub async fn connect(endpoint: &RuntimeEndpoint) -> Result<ClientConnection, ClientError> {
    #[cfg(unix)]
    let result = ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    let result = ClientConnection::connect(&endpoint.pipe_name).await;

    result.map_err(|source| match source.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            ClientError::DaemonNotRunning(source)
        }
        _ => ClientError::Io(source),
    })
}

/// Send this client's [`HandshakeRequest`] and return the daemon's version
/// string on success, or an explicit [`ClientError::VersionMismatch`] /
/// [`ClientError::Rejected`] otherwise -- never a guessed compatibility.
pub async fn handshake(
    connection: &mut ClientConnection,
    client_kind: &str,
) -> Result<String, ClientError> {
    let request = Request::Handshake(HandshakeRequest {
        protocol_version: PROTOCOL_VERSION,
        client_kind: client_kind.to_owned(),
    });
    protocol::framing::write_message(connection, &request)
        .await
        .map_err(ClientError::Io)?;

    let response: Response = protocol::framing::read_message(connection)
        .await
        .map_err(ClientError::Io)?;

    match response {
        Response::Handshake(HandshakeResponse::Ok { daemon_version, .. }) => Ok(daemon_version),
        Response::Handshake(HandshakeResponse::VersionMismatch {
            server_protocol_version,
            client_protocol_version,
        }) => Err(ClientError::VersionMismatch {
            server_protocol_version,
            client_protocol_version,
        }),
        Response::Error(ErrorResponse { message, .. }) => Err(ClientError::Rejected(message)),
        Response::Status(_) | Response::Install(_) | Response::Init(_) => {
            Err(ClientError::UnexpectedResponse)
        }
    }
}

/// Request the daemon's current status. Must be called on a connection
/// that has already completed a successful [`handshake`].
pub async fn status(connection: &mut ClientConnection) -> Result<StatusResponse, ClientError> {
    let request = Request::Status(StatusRequest);
    protocol::framing::write_message(connection, &request)
        .await
        .map_err(ClientError::Io)?;

    let response: Response = protocol::framing::read_message(connection)
        .await
        .map_err(ClientError::Io)?;

    match response {
        Response::Status(status) => Ok(status),
        Response::Error(ErrorResponse { message, .. }) => Err(ClientError::Rejected(message)),
        Response::Handshake(_) | Response::Install(_) | Response::Init(_) => {
            Err(ClientError::UnexpectedResponse)
        }
    }
}

/// Connect and handshake in one call, reporting whether a *live* daemon
/// answered at all -- used only for the startup-time stale-endpoint probe
/// (#13 task 4 §6, §14). A connect failure and a handshake failure are
/// both simply "not live"; the caller does not need to distinguish them.
pub(crate) async fn probe_live(endpoint: &RuntimeEndpoint) -> bool {
    let Ok(mut connection) = connect(endpoint).await else {
        return false;
    };
    handshake(&mut connection, "brainprintd-self-probe")
        .await
        .is_ok()
}
