//! Shared wire mirrors used across several evidence/result shapes.

use serde::{Deserialize, Serialize};

use super::target::{ResourceKindWire, ResourceLanguageWire, ResourceRoleWire};
use crate::ResourceId;

/// #76: the one place a canonical 0-based source line becomes the 1-based
/// editor line a human locator (`path:line`) cites.
pub const fn line_1based(line: usize) -> usize {
    line + 1
}

/// Mirrors `brainprint_engine::parser::SourcePoint`: `line` and `column` stay
/// the canonical 0-based coordinates. Serialized with a derived
/// `line_1based` (#76) so a reader of the wire JSON (MCP) gets the editor
/// line; deserialization ignores it, so peers of either shape interoperate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "SourcePointOut")]
pub struct SourcePointWire {
    pub line: usize,
    pub column: usize,
}

impl SourcePointWire {
    pub const fn line_1based(&self) -> usize {
        line_1based(self.line)
    }

    /// The 1-based column a human `path:line:column` locator cites.
    pub const fn column_1based(&self) -> usize {
        self.column + 1
    }
}

#[derive(Serialize)]
struct SourcePointOut {
    line: usize,
    column: usize,
    line_1based: usize,
}

impl From<SourcePointWire> for SourcePointOut {
    fn from(point: SourcePointWire) -> Self {
        Self {
            line: point.line,
            column: point.column,
            line_1based: point.line_1based(),
        }
    }
}

/// Mirrors `brainprint_engine::parser::SourceSpan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpanWire {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start: SourcePointWire,
    pub end: SourcePointWire,
}

/// Mirrors `brainprint_engine::query::NotCurrentReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotCurrentReasonWire {
    ResourceIndexDirty,
    ResourceIndexNeverPublished,
}

/// Mirrors `brainprint_engine::query::Currentness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CurrentnessWire {
    Current,
    NotCurrent(NotCurrentReasonWire),
}

/// Mirrors `brainprint_engine::query::ResultSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResultSourceWire {
    StructuralIndex,
}

/// Mirrors `brainprint_engine::query::StructuralCoverage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StructuralCoverageWire {
    Complete,
    Partial,
    ContainerOnly,
    Unsupported,
    GeneratedUnmapped,
}

/// Mirrors `brainprint_engine::query::CoverageNote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageNoteWire {
    pub resource_id: ResourceId,
    pub path_rel: String,
    pub coverage: StructuralCoverageWire,
}

/// Mirrors `brainprint_engine::resource::ResourceState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceStateWire {
    Active,
    Deleted,
}

/// Mirrors `brainprint_engine::resource::Resource`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceWire {
    pub id: ResourceId,
    pub path_rel: String,
    pub path_key: String,
    pub kind: ResourceKindWire,
    pub role: ResourceRoleWire,
    pub language: Option<ResourceLanguageWire>,
    pub size_bytes: i64,
    pub mtime_ns: i64,
    pub fingerprint: String,
    pub content_hash: Option<String>,
    pub state: ResourceStateWire,
    pub resource_revision: String,
    pub generated_kind: Option<String>,
    pub container_resource_id: Option<ResourceId>,
}

/// Mirrors `brainprint_engine::symbol::Visibility`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VisibilityWire {
    Public,
    Private,
    Protected,
    Internal,
    Unspecified,
}

/// Mirrors `brainprint_engine::symbol::Symbol`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolWire {
    pub id: crate::SymbolId,
    pub resource_id: ResourceId,
    pub parent_id: Option<crate::SymbolId>,
    pub kind: super::target::SymbolKindWire,
    pub name: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub visibility: VisibilityWire,
    pub exported: bool,
    pub span: SourceSpanWire,
    pub resource_revision: String,
    pub analysis_profile_id: i64,
}

/// Mirrors `brainprint_engine::query::SymbolCandidate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolCandidateWire {
    pub symbol: SymbolWire,
    pub path_rel: String,
    pub coverage: StructuralCoverageWire,
}

/// Mirrors `brainprint_engine::resolution::Support`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupportWire {
    Supported,
    Partial,
    Unsupported,
}

/// Mirrors `brainprint_engine::resolution::Freshness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FreshnessWire {
    Fresh,
    Dirty,
    Stale,
}

/// Mirrors `brainprint_engine::resolution::Dispatch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DispatchWire {
    Static,
    Dynamic,
    Unknown,
}

/// Mirrors `brainprint_engine::resolution::TargetScope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetScopeWire {
    Internal,
    External,
}

/// Mirrors `brainprint_engine::symbol::OccurrenceKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OccurrenceKindWire {
    Definition,
    ImportSite,
    CallSite,
    ReferenceSite,
    TypeSite,
    KeySite,
    CallCandidateSite,
}

/// Mirrors `brainprint_engine::relations::EvidenceLocation`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLocationWire {
    pub resource: ResourceId,
    pub containing_symbol: Option<crate::SymbolId>,
    pub occurrence_kind: OccurrenceKindWire,
    pub span: SourceSpanWire,
    pub basis_revision: String,
    pub support: SupportWire,
    pub freshness: FreshnessWire,
}
