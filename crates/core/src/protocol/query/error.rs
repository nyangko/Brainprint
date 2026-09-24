//! The complete `CoreError` -> wire error mapping (#24 §9).
//!
//! Raw SQLite/driver/debug text never crosses this boundary. Valid
//! semantic outcomes (NotFound, MultipleCandidates, NotCurrent, partial
//! coverage, truncation) are `QueryResultWire` values, never errors here.

use serde::{Deserialize, Serialize};

use crate::WorkspaceId;

/// Mirrors `brainprint_engine::query_surface::NotInitialized`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotInitializedReasonWire {
    GlobalDbMissing,
    WorkspaceNotRegistered,
    WorkspaceDbMissing,
    IndexDbMissing,
    WorkspaceUnbound { db: String },
}

/// Mirrors `brainprint_engine::query_surface::InvalidRequest`'s stable,
/// wire-safe reasons (the underlying `ProjectionRequestError`/
/// `SummaryRequestError` payloads collapse to a coarse reason here: their
/// `Display` text is unstable prose, not a wire contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvalidRequestReasonWire {
    MissingTarget,
    MissingWorkItem,
    EmptySelector,
    InvalidScopeLayer,
    DirectiveOutsideApplicability,
    InvalidDirective,
    EmptyCorrelationId,
    EmptyKnowledgeRef,
    InvalidSummaryRequest,
    ListLimitTooLarge { limit: usize, max: usize },
    SearchBudgetInvalid,
}

/// Mirrors `brainprint_engine::projection::planner::delivery::
/// ContinuationMismatch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContinuationMismatchReasonWire {
    Workspace,
    IndexIncarnation,
    WorkspaceRevision,
    StableGeneration,
    Request,
    Projection,
    Budget,
}

/// The complete #24 §9 mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryErrorCodeWire {
    NotInitialized,
    WorkspaceRootMissing,
    WorkspaceAmbiguous,
    WorkspaceMismatch,
    WorkspaceBindingMismatch,
    WorkItemNotFound,
    InvalidRequest,
    BudgetTooSmall,
    ContinuationMismatch,
    InvalidDeliveryRequest,
    QueryFailed,
    ResultTooLarge,
    DaemonInternal,
}

/// Typed detail for [`QueryErrorCodeWire::ResultTooLarge`] (#24 §10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultTooLargeDetailWire {
    pub encoded_bytes: usize,
    pub max_bytes: usize,
    /// The operation family that produced the oversized result, e.g.
    /// `"relations"`.
    pub operation: String,
}

/// Every typed detail payload a [`QueryErrorCodeWire`] may carry. Exactly
/// one of these (or none) accompanies a given `code`; the adapter never
/// mixes detail shapes across codes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryErrorDetailWire {
    NotInitialized(NotInitializedReasonWire),
    WorkspaceAmbiguous {
        workspaces: Vec<WorkspaceId>,
    },
    WorkspaceMismatch {
        bound: WorkspaceId,
        requested: WorkspaceId,
    },
    WorkspaceBindingMismatch {
        db: String,
        expected: WorkspaceId,
        found: WorkspaceId,
    },
    WorkItemNotFound {
        work_item: crate::WorkItemId,
    },
    InvalidRequest(InvalidRequestReasonWire),
    ContinuationMismatch(ContinuationMismatchReasonWire),
    ResultTooLarge(ResultTooLargeDetailWire),
}

/// Mirrors #24 §9's `QueryErrorWire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryErrorWire {
    pub code: QueryErrorCodeWire,
    pub detail: Option<QueryErrorDetailWire>,
    /// A safe human summary. Never a raw driver/SQLite string (#24 §9).
    pub message: String,
}
