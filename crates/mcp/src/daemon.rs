//! MCP-side connect + handshake + Query/QueryAck helpers (#25 "Crate
//! boundary"). Built only on `brainprint-core::protocol` -- mirrors
//! `brainprint-cli::client` by design (the two are independent
//! implementations of the same Task 11 wire contract, not a shared crate;
//! #25 explicitly allows a small MCP-local helper and asks for a new
//! shared client crate only if duplication material blocks correctness,
//! which it does not here).

use std::{fmt, io};

use brainprint_core::{
    PROTOCOL_VERSION, WorkspaceId,
    protocol::{
        self, ClientConnection, EndpointPaths, EndpointResolutionError, ErrorResponse,
        HandshakeRequest, HandshakeResponse, Request, Response, StatusRequest, StatusResponse,
        query::{
            CorrelationWire, QueryAckRequest, QueryAckResponse, QueryOperationWire, QueryRequest,
            QueryResponse, WorkspaceSelectorWire,
        },
    },
};

const CLIENT_KIND: &str = "brainprint-mcp";

/// Layer 2 of #25's three error layers: daemon unavailable, protocol
/// mismatch, local IPC failure. Never a raw driver/debug dump -- the
/// daemon has already mapped anything unsafe onto a stable `ErrorKind`/
/// message before it reaches the wire.
#[derive(Debug)]
pub enum DaemonError {
    EndpointUnavailable(EndpointResolutionError),
    DaemonNotRunning,
    Io(io::Error),
    VersionMismatch {
        server_protocol_version: u32,
        client_protocol_version: u32,
    },
    UnexpectedResponse,
    Rejected {
        message: String,
    },
}

impl fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndpointUnavailable(source) => write!(formatter, "{source}"),
            Self::DaemonNotRunning => formatter.write_str(
                "brainprintd is not running -- start it, then retry (e.g. `brainprintd &`)",
            ),
            Self::Io(source) => {
                write!(formatter, "communication with brainprintd failed: {source}")
            }
            Self::VersionMismatch {
                server_protocol_version,
                client_protocol_version,
            } => write!(
                formatter,
                "protocol version mismatch: brainprintd speaks {server_protocol_version}, \
                 brainprint-mcp speaks {client_protocol_version} -- update brainprint/brainprintd \
                 to matching versions"
            ),
            Self::UnexpectedResponse => {
                formatter.write_str("brainprintd sent an unexpected response for this request")
            }
            Self::Rejected { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DaemonError {}

/// Resolve the daemon endpoint, connect, and handshake. Every tool call
/// starts here -- a fresh connection per call, not a pooled/shared one:
/// the daemon already owns one worker thread per Workspace (#24 §14), so
/// nothing here needs to be, and a fresh connection keeps concurrent MCP
/// calls trivially independent.
/// `endpoint_override` lets a caller name an exact daemon endpoint
/// instead of resolving it from the real ambient environment --
/// `BrainprintMcp`'s normal (production) path always passes `None`, so
/// it always resolves via [`EndpointPaths::resolve`], identically to
/// `brainprint-cli`. The override exists solely so integration tests can
/// point an in-process `BrainprintMcp` at an isolated, fixture-scoped
/// daemon without mutating the real process environment (unsound to do
/// across concurrently-running tests, and `unsafe_code` is denied
/// workspace-wide regardless).
pub async fn connect_and_handshake(
    endpoint_override: Option<&EndpointPaths>,
) -> Result<ClientConnection, DaemonError> {
    let resolved;
    let endpoint = match endpoint_override {
        Some(endpoint) => endpoint,
        None => {
            resolved = EndpointPaths::resolve().map_err(DaemonError::EndpointUnavailable)?;
            &resolved
        }
    };

    #[cfg(unix)]
    let connect_result = ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    let connect_result = ClientConnection::connect(&endpoint.pipe_name).await;

    let mut connection = connect_result.map_err(|source| match source.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => DaemonError::DaemonNotRunning,
        _ => DaemonError::Io(source),
    })?;

    let handshake_request = Request::Handshake(HandshakeRequest {
        protocol_version: PROTOCOL_VERSION,
        client_kind: CLIENT_KIND.to_owned(),
    });
    match send(&mut connection, handshake_request).await? {
        Response::Handshake(HandshakeResponse::Ok { .. }) => Ok(connection),
        Response::Handshake(HandshakeResponse::VersionMismatch {
            server_protocol_version,
            client_protocol_version,
        }) => Err(DaemonError::VersionMismatch {
            server_protocol_version,
            client_protocol_version,
        }),
        Response::Error(ErrorResponse { message, .. }) => Err(DaemonError::Rejected { message }),
        _ => Err(DaemonError::UnexpectedResponse),
    }
}

pub async fn query(
    connection: &mut ClientConnection,
    request_id: String,
    workspace: WorkspaceSelectorWire,
    correlation: Option<CorrelationWire>,
    operation: QueryOperationWire,
) -> Result<QueryResponse, DaemonError> {
    let request = Request::Query(QueryRequest {
        request_id,
        workspace,
        correlation,
        operation,
    });
    match send(connection, request).await? {
        Response::Query(response) => Ok(response),
        Response::Error(ErrorResponse { message, .. }) => Err(DaemonError::Rejected { message }),
        _ => Err(DaemonError::UnexpectedResponse),
    }
}

/// Sent only after the MCP adapter has successfully received and decoded
/// the `QueryResponse` (#25 "Retention / acknowledgement"). A failure
/// here is diagnostic, not fatal to the tool call already answered.
pub async fn ack(
    connection: &mut ClientConnection,
    request_id: String,
    workspace_id: WorkspaceId,
    ack_token: String,
) -> Result<QueryAckResponse, DaemonError> {
    let request = Request::QueryAck(QueryAckRequest {
        request_id,
        workspace_id,
        ack_token,
    });
    match send(connection, request).await? {
        Response::QueryAck(response) => Ok(response),
        Response::Error(ErrorResponse { message, .. }) => Err(DaemonError::Rejected { message }),
        _ => Err(DaemonError::UnexpectedResponse),
    }
}

pub async fn status(connection: &mut ClientConnection) -> Result<StatusResponse, DaemonError> {
    match send(connection, Request::Status(StatusRequest)).await? {
        Response::Status(status) => Ok(status),
        Response::Error(ErrorResponse { message, .. }) => Err(DaemonError::Rejected { message }),
        _ => Err(DaemonError::UnexpectedResponse),
    }
}

async fn send(
    connection: &mut ClientConnection,
    request: Request,
) -> Result<Response, DaemonError> {
    protocol::framing::write_message(connection, &request)
        .await
        .map_err(DaemonError::Io)?;
    protocol::framing::read_message(connection)
        .await
        .map_err(DaemonError::Io)
}
