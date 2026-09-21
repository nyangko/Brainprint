//! Wire message types for the local `brainprint`/`brainprintd` protocol
//! (#15 task 9 / #13 task 3 §8, task 4).
//!
//! These are plain, versioned data contracts -- no I/O, no transport. A
//! client's very first message is always [`HandshakeRequest`]; a daemon
//! that has not yet handshaken a connection only ever answers
//! [`Response::Handshake`] on it (see [`crate::protocol::framing`] and
//! [`crate::protocol::transport`] for how these move over the wire).

use serde::{Deserialize, Serialize};

/// A client's opening message on a freshly connected IPC stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeRequest {
    /// The client's [`crate::PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Free-form client identity for diagnostics (e.g. `"brainprint-cli"`,
    /// `"brainprint-mcp"`). Never used for authorization in P0.
    pub client_kind: String,
}

/// The daemon's reply to a [`HandshakeRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeResponse {
    /// Protocol versions are compatible; the connection may proceed to
    /// further requests.
    Ok {
        protocol_version: u32,
        daemon_version: String,
    },
    /// The client's protocol version is not one this daemon understands.
    /// The daemon does not guess compatibility -- any mismatch is
    /// rejected explicitly rather than silently accepted.
    VersionMismatch {
        server_protocol_version: u32,
        client_protocol_version: u32,
    },
}

/// Request the daemon's current runtime status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRequest;

/// The daemon's runtime status.
///
/// #15 task 9: only information the daemon actually tracks in I1 --
/// process identity, build/protocol identity, and how long it has been
/// running. No I2+ state (Workspace/generation/watcher status) is
/// reported here; that would be fabricating state this daemon does not
/// yet own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub daemon_version: String,
    pub protocol_version: u32,
    pub pid: u32,
    /// Milliseconds since the Unix epoch when the daemon process started.
    pub started_at_unix_ms: u64,
    pub uptime_seconds: u64,
}

/// Bootstrap/migrate global config + `global.db` through the daemon (#15
/// task 10). Carries no fields: `install` always targets this user's
/// single global install, never a Workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRequest;

/// Idempotent: `config_freshly_created`/`db_freshly_created` are `false`
/// on a repeat call that found an already-bootstrapped install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallResponse {
    pub global_config_path: String,
    pub global_db_path: String,
    pub config_freshly_created: bool,
    pub db_freshly_created: bool,
}

/// Init `path` as a Brainprint Workspace through the daemon (#15 task 10),
/// which runs it through the existing `engine::init::init_workspace` --
/// fresh Git/non-Git (task 6) and secondary Git worktree (task 7) routing
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitRequest {
    /// Resolved to an absolute path by the *client* before sending: the
    /// daemon's working directory is unrelated to the caller's, so a
    /// relative path here would be ambiguous.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitResponse {
    /// Canonical UUID string form (see `brainprint_core::id`).
    pub project_id: String,
    pub workspace_id: String,
    pub workspace_root: String,
    pub is_git: bool,
    pub freshly_created: bool,
}

/// An envelope for every request a client may send after a successful
/// handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Handshake(HandshakeRequest),
    Status(StatusRequest),
    Install(InstallRequest),
    Init(InitRequest),
}

/// An envelope for every response the daemon may send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Handshake(HandshakeResponse),
    Status(StatusResponse),
    Install(InstallResponse),
    Init(InitResponse),
    /// The daemon rejected the request itself (e.g. a request sent before
    /// handshake completed, or `init` hit an identity/registry conflict).
    /// Never used to smuggle a fabricated success.
    Error(ErrorResponse),
}

/// A coarse, stable classification a client can act on without parsing
/// `message` text (#15 task 10: mapped once at the daemon boundary so a
/// raw `rusqlite`/internal error never reaches the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    /// The request itself was malformed (e.g. an unreadable path) --
    /// retrying identically will not help.
    InvalidRequest,
    /// An identity/registry conflict the caller can act on: mismatched
    /// Project/Workspace identity, a missing project-home store, a
    /// duplicate WorkspaceID at a different locator, and the like.
    Conflict,
    /// An internal daemon/storage failure. `message` is a stable, safe
    /// summary -- never a raw driver/SQLite error string (that is logged
    /// daemon-side only).
    DaemonInternal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub kind: ErrorKind,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_json() {
        let request = Request::Handshake(HandshakeRequest {
            protocol_version: 1,
            client_kind: "brainprint-cli".to_owned(),
        });

        let encoded = serde_json::to_vec(&request).expect("request should serialize");
        let decoded: Request = serde_json::from_slice(&encoded).expect("request should decode");

        assert_eq!(decoded, request);
    }

    #[test]
    fn response_round_trips_through_json() {
        let response = Response::Status(StatusResponse {
            daemon_version: "0.1.0".to_owned(),
            protocol_version: 1,
            pid: 4242,
            started_at_unix_ms: 1_700_000_000_000,
            uptime_seconds: 12,
        });

        let encoded = serde_json::to_vec(&response).expect("response should serialize");
        let decoded: Response = serde_json::from_slice(&encoded).expect("response should decode");

        assert_eq!(decoded, response);
    }

    #[test]
    fn init_request_round_trips_through_json() {
        let request = Request::Init(InitRequest {
            path: "/repo/main".to_owned(),
        });

        let encoded = serde_json::to_vec(&request).expect("request should serialize");
        let decoded: Request = serde_json::from_slice(&encoded).expect("request should decode");

        assert_eq!(decoded, request);
    }

    #[test]
    fn error_response_round_trips_through_json() {
        let response = Response::Error(ErrorResponse {
            kind: ErrorKind::Conflict,
            message: "workspace identity mismatch".to_owned(),
        });

        let encoded = serde_json::to_vec(&response).expect("response should serialize");
        let decoded: Response = serde_json::from_slice(&encoded).expect("response should decode");

        assert_eq!(decoded, response);
    }
}
