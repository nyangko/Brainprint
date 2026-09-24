//! Wire mirror of the Task 7/8 delivery contract (#24 §6, §7).
//!
//! `DeliveryContinuationWire` is a lossless typed reconstruction of the
//! existing `brainprint_engine` `DeliveryContinuation` -- never a second
//! server cursor. The daemon adapter is the only place that converts
//! between this and the engine type, through the read-only
//! accessors/checked constructor `brainprint-engine` exposes for exactly
//! this bridge.

use serde::{Deserialize, Serialize};

use crate::{IndexIncarnationId, WorkspaceId};

/// Mirrors the delivery inputs of a planner-backed operation (#24 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryWire {
    pub budget: DeliveryBudgetWire,
    pub continuation: Option<DeliveryContinuationWire>,
    pub retention: RetentionWire,
}

/// Mirrors `brainprint_engine::projection::planner::DeliveryBudget`, minus
/// `max_tokens`: Level 0 has no exact token counter (#24 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryBudgetWire {
    pub max_items: Option<std::num::NonZeroUsize>,
    pub max_bytes: Option<std::num::NonZeroUsize>,
}

/// Mirrors `brainprint_engine::projection::planner::ContextRetention`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionWire {
    Retained,
    Fresh,
    Disabled,
}

/// A lossless wire reconstruction of
/// `brainprint_engine::projection::planner::DeliveryContinuation` (#24 §7).
/// `budget` here keeps its own optional `max_tokens` field so a
/// continuation this adapter never produced but a future caller supplies
/// still round-trips exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryContinuationWire {
    pub workspace_id: WorkspaceId,
    pub index_incarnation_id: IndexIncarnationId,
    pub workspace_revision: String,
    pub generation_no: i64,
    pub generation_basis_revision: String,
    /// 64-char lowercase hex (SHA-256).
    pub request_fingerprint: String,
    /// 64-char lowercase hex (SHA-256).
    pub projection_fingerprint: String,
    pub budget: ContinuationBudgetWire,
    pub next: DeliveryKeyWire,
}

/// The continuation's own budget snapshot, which may carry `max_tokens`
/// even though Task 11 never sets it on a request (#24 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuationBudgetWire {
    pub max_items: Option<std::num::NonZeroUsize>,
    pub max_bytes: Option<std::num::NonZeroUsize>,
    pub max_tokens: Option<std::num::NonZeroUsize>,
}

/// Mirrors `brainprint_engine::projection::planner::delivery::DeliveryKey`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryKeyWire {
    pub tier: u8,
    pub depth: usize,
    /// 64-char lowercase hex (SHA-256).
    pub identity: String,
}
