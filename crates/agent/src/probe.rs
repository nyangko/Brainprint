//! Cheap, no-source Task 11 probes (#26 "Cheap probe boundary").
//!
//! The only operation used is a small bounded `Find::Files` over the
//! public local IPC -- index rows (path + `resource_revision`) and the
//! index's own `Currentness`, never source bytes, never a text scan,
//! never a relation/structure materialization, never a backend start.
//! Built only on `brainprint-core::protocol` (#26: no engine/daemon/mcp
//! dependency), mirroring `brainprint-mcp::daemon` by design.

use std::{
    io,
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use brainprint_core::{
    PROTOCOL_VERSION, WorkspaceId,
    protocol::{
        self, ClientConnection, EndpointPaths, HandshakeRequest, HandshakeResponse, Request,
        Response,
        query::{
            CurrentnessWire, FindQueryWire, FindResultWire, QueryErrorCodeWire, QueryOperationWire,
            QueryOutcomeWire, QueryRequest, QueryResultWire, ResourceKindWire,
            WorkspaceSelectorWire,
        },
    },
};

use crate::{event::FallbackReason, util};

const CLIENT_KIND: &str = "brainprint-agent";

/// Hard ceiling for one probe, connect included. A slow daemon means the
/// native action simply proceeds (#26 "Failure policy": fail open).
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// A bounded `Find::Files` request, in Brainprint's own vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesProbe {
    /// Canonical absolute Workspace root, sent as an exact Locator.
    pub root: String,
    pub directory: Option<String>,
    pub recursive: bool,
    pub path_prefix: Option<String>,
    pub limit: usize,
}

/// The compact facts a probe returns. No source, no payload retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub workspace_id: WorkspaceId,
    pub current: bool,
    pub truncated: bool,
    /// In index order.
    pub entries: Vec<ListedResource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedResource {
    /// `ResourceId`'s canonical text -- globally unique, so it ties a
    /// delivery to exactly one Workspace.
    pub id: String,
    pub path_rel: String,
    pub revision: String,
    pub is_file: bool,
}

impl Listing {
    /// Order-sensitive digest of every entry's identity + revision.
    pub fn fingerprint(&self) -> String {
        util::fingerprint(self.entries.iter().flat_map(|entry| {
            [
                entry.id.as_str(),
                entry.path_rel.as_str(),
                entry.revision.as_str(),
            ]
        }))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeStats {
    pub count: u32,
    pub total_micros: u64,
}

pub trait Probe {
    fn files(&mut self, request: &FilesProbe) -> Result<Listing, FallbackReason>;
    fn stats(&self) -> ProbeStats;
}

/// The production probe: a fresh local IPC connection per call.
pub struct IpcProbe {
    endpoint: Option<EndpointPaths>,
    runtime: Option<tokio::runtime::Runtime>,
    stats: ProbeStats,
}

impl IpcProbe {
    /// `endpoint` is `None` in production (resolved from the ambient
    /// environment exactly like `brainprint-cli`/`brainprint-mcp`).
    pub fn new(endpoint: Option<EndpointPaths>) -> Self {
        Self {
            endpoint,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .ok(),
            stats: ProbeStats::default(),
        }
    }
}

impl Probe for IpcProbe {
    fn files(&mut self, request: &FilesProbe) -> Result<Listing, FallbackReason> {
        let started = Instant::now();
        let result = match &self.runtime {
            Some(runtime) => runtime.block_on(async {
                tokio::time::timeout(
                    PROBE_TIMEOUT,
                    files_over_ipc(self.endpoint.as_ref(), request),
                )
                .await
                .unwrap_or(Err(FallbackReason::DaemonUnavailable))
            }),
            None => Err(FallbackReason::BridgeError),
        };
        self.stats.count += 1;
        self.stats.total_micros += util::micros(started.elapsed());
        result
    }

    fn stats(&self) -> ProbeStats {
        self.stats
    }
}

async fn files_over_ipc(
    endpoint: Option<&EndpointPaths>,
    request: &FilesProbe,
) -> Result<Listing, FallbackReason> {
    let limit = NonZeroUsize::new(request.limit).ok_or(FallbackReason::BridgeError)?;
    let resolved;
    let endpoint = match endpoint {
        Some(endpoint) => endpoint,
        None => {
            resolved = EndpointPaths::resolve().map_err(|_| FallbackReason::DaemonUnavailable)?;
            &resolved
        }
    };
    #[cfg(unix)]
    let connected = ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    let connected = ClientConnection::connect(&endpoint.pipe_name).await;
    let mut connection = connected.map_err(|_| FallbackReason::DaemonUnavailable)?;

    let handshake = Request::Handshake(HandshakeRequest {
        protocol_version: PROTOCOL_VERSION,
        client_kind: CLIENT_KIND.to_owned(),
    });
    match send(&mut connection, handshake).await? {
        Response::Handshake(HandshakeResponse::Ok { .. }) => {}
        _ => return Err(FallbackReason::DaemonUnavailable),
    }

    let query = Request::Query(QueryRequest {
        request_id: util::request_id(),
        workspace: WorkspaceSelectorWire::Locator {
            path: request.root.clone(),
        },
        correlation: None,
        operation: QueryOperationWire::Find(FindQueryWire::Files {
            directory: request.directory.clone(),
            recursive: request.recursive,
            path_prefix: request.path_prefix.clone(),
            role: None,
            language: None,
            kind: None,
            limit,
        }),
    });
    let Response::Query(response) = send(&mut connection, query).await? else {
        return Err(FallbackReason::DaemonUnavailable);
    };
    // `Find::Files` is non-paged: no ack_token, nothing retained.
    match response.outcome {
        QueryOutcomeWire::Ok(QueryResultWire::Find(FindResultWire::Files(listing))) => {
            Ok(Listing {
                workspace_id: response.workspace_id,
                current: listing.currentness == CurrentnessWire::Current,
                truncated: listing.truncated,
                entries: listing
                    .entries
                    .into_iter()
                    .map(|entry| ListedResource {
                        id: entry.id.to_string(),
                        path_rel: entry.path_rel,
                        revision: entry.resource_revision,
                        is_file: entry.kind == ResourceKindWire::File,
                    })
                    .collect(),
            })
        }
        QueryOutcomeWire::Ok(_) => Err(FallbackReason::BridgeError),
        QueryOutcomeWire::Err(error) => Err(fallback_for_code(error.code)),
    }
}

async fn send(
    connection: &mut ClientConnection,
    request: Request,
) -> Result<Response, FallbackReason> {
    let io_failed = |_: io::Error| FallbackReason::DaemonUnavailable;
    protocol::framing::write_message(connection, &request)
        .await
        .map_err(io_failed)?;
    protocol::framing::read_message(connection)
        .await
        .map_err(io_failed)
}

/// Map a typed Task 11 error onto the closed #26 fallback vocabulary.
pub const fn fallback_for_code(code: QueryErrorCodeWire) -> FallbackReason {
    match code {
        QueryErrorCodeWire::NotInitialized | QueryErrorCodeWire::WorkspaceRootMissing => {
            FallbackReason::NotInitialized
        }
        QueryErrorCodeWire::WorkspaceAmbiguous => FallbackReason::Ambiguous,
        QueryErrorCodeWire::WorkspaceMismatch | QueryErrorCodeWire::WorkspaceBindingMismatch => {
            FallbackReason::OutsideWorkspace
        }
        QueryErrorCodeWire::ResultTooLarge => FallbackReason::Truncated,
        // This adapter's own request was rejected: a bridge problem.
        QueryErrorCodeWire::InvalidRequest
        | QueryErrorCodeWire::InvalidDeliveryRequest
        | QueryErrorCodeWire::BudgetTooSmall
        | QueryErrorCodeWire::ContinuationMismatch
        | QueryErrorCodeWire::WorkItemNotFound => FallbackReason::BridgeError,
        QueryErrorCodeWire::QueryFailed | QueryErrorCodeWire::DaemonInternal => {
            FallbackReason::DaemonUnavailable
        }
    }
}
