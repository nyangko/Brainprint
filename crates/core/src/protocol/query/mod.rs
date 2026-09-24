//! Task 11 query protocol wire contract (#24 "Implementation contract
//! locked").
//!
//! Wire types only: transport DTOs that mirror an existing Task 10/Core
//! field or carry adapter-only transport metadata (`request_id`,
//! `ack_token`, correlation). Nothing here adds ranking, precedence,
//! ambiguity resolution, or a second budget/continuation/ledger engine --
//! that logic stays in `brainprint-engine`, and this crate does not (and
//! must not) depend on it. The daemon adapter owns every conversion
//! between these types and the engine's Task 10 types.

mod common;
mod delivery;
mod error;
mod evidence;
mod knowledge;
mod operation;
mod relations;
mod result;
mod target;

pub use common::*;
pub use delivery::*;
pub use error::*;
pub use evidence::*;
pub use knowledge::*;
pub use operation::*;
pub use relations::*;
pub use result::*;
pub use target::*;

use serde::{Deserialize, Serialize};

use crate::WorkspaceId;

/// How the caller names the Workspace a query runs against (#24 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceSelectorWire {
    /// An absolute, client-normalized path. The daemon resolves it
    /// through the same `resolve_workspace` every CLI call uses.
    Locator { path: String },
    /// An explicit Workspace identity, for a structured local-API caller
    /// that already has one.
    Id { workspace_id: WorkspaceId },
}

/// Mirrors `brainprint_engine::projection::ProjectionCorrelation`, minus
/// `persona_traits`' set semantics (order/duplicates do not matter on the
/// wire; the adapter reconstructs the set).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrelationWire {
    pub client_id: Option<String>,
    pub session_id: Option<String>,
    pub external_task_id: Option<String>,
    pub external_subtask_id: Option<String>,
    pub role_hint: Option<String>,
    pub team_hint: Option<String>,
    #[serde(default)]
    pub persona_traits: Vec<String>,
}

/// One query request (#24 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRequest {
    /// Canonical UUID string; transport correlation only, never a domain
    /// identity.
    pub request_id: String,
    pub workspace: WorkspaceSelectorWire,
    pub correlation: Option<CorrelationWire>,
    pub operation: QueryOperationWire,
}

/// Either a successful `QueryResultWire` or a typed `QueryErrorWire`
/// (#24 §3, §9). Ordinary Core answer states (`NotFound`, `Ambiguous`,
/// `NotCurrent`, partial coverage, truncation) are `Ok` values here, never
/// `Err`.
// One per CLI/IPC call, never a hot loop; see `Request`'s
// `large_enum_variant` allow in `messages.rs`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryOutcomeWire {
    Ok(QueryResultWire),
    Err(QueryErrorWire),
}

/// The response to one [`QueryRequest`] (#24 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResponse {
    pub request_id: String,
    pub workspace_id: WorkspaceId,
    pub outcome: QueryOutcomeWire,
    /// Present only when `outcome` carries a Task 8 `PendingDelivery`
    /// (i.e. a planner-backed operation's successful result). Non-paged
    /// operations (`Find::Files`, `Find::Text`, `Relations`, `Knowledge`,
    /// `Structure`) never carry one.
    pub ack_token: Option<String>,
}

/// Acknowledge receipt of a [`QueryResponse`]'s `ack_token` (#24 §3, §15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryAckRequest {
    pub request_id: String,
    pub workspace_id: WorkspaceId,
    pub ack_token: String,
}

/// Mirrors the #24 §15 acknowledgement state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AckStatusWire {
    Acknowledged,
    AlreadyAcknowledged,
    UnknownOrExpired,
}

/// The response to one [`QueryAckRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryAckResponse {
    pub request_id: String,
    pub status: AckStatusWire,
}
