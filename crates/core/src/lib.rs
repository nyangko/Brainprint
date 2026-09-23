//! Canonical Brainprint types and protocol contracts.

pub mod build;
pub mod id;
pub mod protocol;

pub use build::{BuildInfo, PACKAGE_VERSION, PRODUCT_NAME, PROTOCOL_VERSION};
pub use id::{
    BlueprintApplicationId, BlueprintId, DecisionId, IndexIncarnationId, LogicalSymbolId,
    ParseStableIdError, PolicyId, ProjectId, ProjectStateId, ResourceId, SymbolId,
    UserPreferenceId, WorkItemId, WorkNoteId, WorkspaceId,
};
