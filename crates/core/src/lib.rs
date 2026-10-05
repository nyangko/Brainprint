//! Canonical Brainprint types and protocol contracts.

pub mod build;
pub mod id;
pub mod present;
pub mod protocol;

pub use build::{BuildInfo, PACKAGE_VERSION, PRODUCT_NAME, PROTOCOL_VERSION};
pub use id::{
    BlueprintApplicationId, BlueprintId, DecisionId, IndexIncarnationId, LogicalSymbolId,
    ParseStableIdError, PolicyId, ProjectId, ProjectStateId, PromotionId, ResourceId, SymbolId,
    UserPreferenceId, VerificationJobId, WorkItemId, WorkNoteId, WorkspaceId,
};
