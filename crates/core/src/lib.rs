//! Canonical Brainprint types and protocol contracts.

pub mod build;
pub mod id;
pub mod protocol;

pub use build::{BuildInfo, PACKAGE_VERSION, PRODUCT_NAME, PROTOCOL_VERSION};
pub use id::{ParseStableIdError, ProjectId, ResourceId, WorkspaceId};
