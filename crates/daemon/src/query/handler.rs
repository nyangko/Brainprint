//! Top-level `Request::Query`/`Request::QueryAck` dispatch (#24 §3, §14).

use std::path::PathBuf;

use brainprint_core::{
    WorkspaceId,
    protocol::{
        Response,
        query::{
            AckStatusWire, QueryAckRequest, QueryAckResponse, QueryOutcomeWire, QueryRequest,
            QueryResponse, WorkspaceSelectorWire,
        },
    },
};

use super::runtime::{AckOutcome, DaemonQueryRuntime};

/// Sentinel used only when a `QueryResponse` must report an error that
/// occurred *before* a Workspace locator resolved to a real identity
/// (`NOT_INITIALIZED`, `WORKSPACE_AMBIGUOUS`): the wire contract's
/// `workspace_id` field is not optional, and no real identity exists yet
/// to put there. Indistinguishable in practice from any real (v4 random)
/// `WorkspaceId`, so it never collides with one.
fn unresolved_workspace_sentinel() -> WorkspaceId {
    WorkspaceId::from_bytes([0; 16])
}

pub async fn handle_query(runtime: &DaemonQueryRuntime, request: QueryRequest) -> Response {
    let echo_id = match &request.workspace {
        WorkspaceSelectorWire::Id { workspace_id } => Some(*workspace_id),
        WorkspaceSelectorWire::Locator { .. } => None,
    };
    let workspace_id = match request.workspace {
        WorkspaceSelectorWire::Id { workspace_id } => Ok(workspace_id),
        WorkspaceSelectorWire::Locator { path } => {
            runtime.resolve_workspace(PathBuf::from(path)).await
        }
    };

    let workspace_id = match workspace_id {
        Ok(id) => id,
        Err(error) => {
            return Response::Query(QueryResponse {
                request_id: request.request_id,
                workspace_id: echo_id.unwrap_or_else(unresolved_workspace_sentinel),
                outcome: QueryOutcomeWire::Err(error),
                ack_token: None,
            });
        }
    };

    let outcome = runtime
        .query(workspace_id, request.operation, request.correlation)
        .await;
    let (outcome, ack_token) = match outcome {
        Ok((result, ack_token)) => (QueryOutcomeWire::Ok(result), ack_token),
        Err(error) => (QueryOutcomeWire::Err(error), None),
    };

    Response::Query(QueryResponse {
        request_id: request.request_id,
        workspace_id,
        outcome,
        ack_token,
    })
}

pub async fn handle_query_ack(runtime: &DaemonQueryRuntime, request: QueryAckRequest) -> Response {
    let status = match runtime.ack(request.workspace_id, request.ack_token).await {
        AckOutcome::Acknowledged => AckStatusWire::Acknowledged,
        AckOutcome::AlreadyAcknowledged => AckStatusWire::AlreadyAcknowledged,
        AckOutcome::UnknownOrExpired => AckStatusWire::UnknownOrExpired,
    };
    Response::QueryAck(QueryAckResponse {
        request_id: request.request_id,
        status,
    })
}
