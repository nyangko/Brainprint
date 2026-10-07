//! CLI-side connect + handshake + request helpers.
//!
//! Built only on `brainprint-core::protocol` -- #15 task 10 requires
//! `brainprint-cli -> brainprint-core` as the only dependency direction,
//! so this deliberately does **not** import `brainprint-daemon::client`
//! even though the shape looks similar; the two are independent
//! implementations of the same wire contract by design.

use std::{fmt, io};

use brainprint_core::{
    PROTOCOL_VERSION, lifecycle,
    protocol::{
        self, ClientConnection, EndpointPaths, EndpointResolutionError, ErrorKind, ErrorResponse,
        HandshakeRequest, HandshakeResponse, InitRequest, InitResponse, InstallRequest,
        InstallResponse, Request, Response, StatusRequest, StatusResponse,
    },
};

const CLIENT_KIND: &str = "brainprint-cli";

/// A user-facing failure talking to `brainprintd`. `Display` never
/// includes a raw driver/debug dump -- the daemon has already mapped
/// anything unsafe onto a stable [`ErrorKind`]/message before it reaches
/// the wire (see `brainprint-daemon::handlers`).
#[derive(Debug)]
pub enum CliError {
    EndpointUnavailable(EndpointResolutionError),
    DaemonNotRunning,
    Io(io::Error),
    VersionMismatch {
        server_protocol_version: u32,
        client_protocol_version: u32,
    },
    UnexpectedResponse,
    Rejected {
        #[allow(dead_code)]
        kind: ErrorKind,
        message: String,
    },
    /// #89: no daemon was running, and starting one failed or is off.
    AutoStart(lifecycle::LifecycleError),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndpointUnavailable(source) => write!(formatter, "{source}"),
            Self::DaemonNotRunning => formatter
                .write_str("brainprintd is not running -- start it with `brainprint daemon start`"),
            Self::Io(source) => {
                write!(formatter, "communication with brainprintd failed: {source}")
            }
            Self::VersionMismatch {
                server_protocol_version,
                client_protocol_version,
            } => {
                write!(
                    formatter,
                    "protocol version mismatch: brainprintd speaks {server_protocol_version}, this CLI speaks {client_protocol_version} -- "
                )?;
                // #66: say which side is stale. A CLI is a fresh process
                // per command, so there is nothing to reconnect.
                if server_protocol_version < client_protocol_version {
                    formatter.write_str(
                        "the running brainprintd is older: stop it and start the matching brainprintd",
                    )
                } else {
                    formatter.write_str("this brainprint CLI is older: run the matching brainprint")
                }
            }
            Self::UnexpectedResponse => {
                formatter.write_str("brainprintd sent an unexpected response for this request")
            }
            Self::Rejected { message, .. } => formatter.write_str(message),
            Self::AutoStart(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for CliError {}

/// Resolve the daemon endpoint, connect, and handshake -- starting the
/// daemon in the background first if none is running (#89). Every
/// daemon-dependent command starts here.
pub async fn connect_and_handshake() -> Result<ClientConnection, CliError> {
    let endpoint = EndpointPaths::resolve().map_err(CliError::EndpointUnavailable)?;
    match connect(&endpoint).await {
        Err(CliError::DaemonNotRunning) => {
            lifecycle::ensure_started(&endpoint)
                .await
                .map_err(CliError::AutoStart)?;
            connect(&endpoint).await
        }
        result => result,
    }
}

/// [`connect_and_handshake`] without the auto-start: for `status`, which
/// reports the daemon as it is and never starts anything.
pub async fn connect_existing() -> Result<ClientConnection, CliError> {
    let endpoint = EndpointPaths::resolve().map_err(CliError::EndpointUnavailable)?;
    connect(&endpoint).await
}

/// #89: start the daemon if none answers `endpoint` -- for the TUI/Web,
/// which then open one connection per request through [`connect`].
pub async fn ensure_daemon(endpoint: &EndpointPaths) -> Result<(), CliError> {
    match connect(endpoint).await {
        Err(CliError::DaemonNotRunning) => lifecycle::ensure_started(endpoint)
            .await
            .map_err(CliError::AutoStart),
        result => result.map(drop),
    }
}

/// Connect to `endpoint` and handshake. #70: the TUI holds one endpoint
/// and opens a fresh connection per request through this.
pub async fn connect(endpoint: &EndpointPaths) -> Result<ClientConnection, CliError> {
    #[cfg(unix)]
    let connect_result = ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    let connect_result = ClientConnection::connect(&endpoint.pipe_name).await;

    let mut connection = connect_result.map_err(|source| match source.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => CliError::DaemonNotRunning,
        _ => CliError::Io(source),
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
        }) => Err(CliError::VersionMismatch {
            server_protocol_version,
            client_protocol_version,
        }),
        Response::Error(ErrorResponse { kind, message }) => {
            Err(CliError::Rejected { kind, message })
        }
        _ => Err(CliError::UnexpectedResponse),
    }
}

/// The daemon's status; with `path` (absolute), also that Workspace's.
pub async fn status(
    connection: &mut ClientConnection,
    path: Option<String>,
) -> Result<StatusResponse, CliError> {
    match send(connection, Request::Status(StatusRequest { path })).await? {
        Response::Status(status) => Ok(status),
        Response::Error(ErrorResponse { kind, message }) => {
            Err(CliError::Rejected { kind, message })
        }
        _ => Err(CliError::UnexpectedResponse),
    }
}

pub async fn install(connection: &mut ClientConnection) -> Result<InstallResponse, CliError> {
    match send(connection, Request::Install(InstallRequest)).await? {
        Response::Install(install) => Ok(install),
        Response::Error(ErrorResponse { kind, message }) => {
            Err(CliError::Rejected { kind, message })
        }
        _ => Err(CliError::UnexpectedResponse),
    }
}

pub async fn init(
    connection: &mut ClientConnection,
    path: String,
) -> Result<InitResponse, CliError> {
    match send(connection, Request::Init(InitRequest { path })).await? {
        Response::Init(init) => Ok(init),
        Response::Error(ErrorResponse { kind, message }) => {
            Err(CliError::Rejected { kind, message })
        }
        _ => Err(CliError::UnexpectedResponse),
    }
}

pub(crate) async fn send(
    connection: &mut ClientConnection,
    request: Request,
) -> Result<Response, CliError> {
    protocol::framing::write_message(connection, &request)
        .await
        .map_err(CliError::Io)?;
    protocol::framing::read_message(connection)
        .await
        .map_err(CliError::Io)
}
