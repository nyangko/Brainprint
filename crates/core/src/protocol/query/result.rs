//! Wire mirror of the seven Task 10 operations' result shapes (#24 §8).

use serde::{Deserialize, Serialize};

use super::{
    common::{CurrentnessWire, ResourceWire, ResultSourceWire, SourceSpanWire},
    delivery::DeliveryContinuationWire,
    evidence::{EvidenceWire, ProjectionGapWire, RangeRoleWire, TargetResolutionWire},
    knowledge::KnowledgeResultWire,
    relations::RelationAnswerWire,
    target::{
        GraphEndpointWire, RelationKindWire, ResourceKindWire, ResourceLanguageWire,
        ResourceRoleWire,
    },
};
use crate::{
    BlueprintApplicationId, DecisionId, PolicyId, ProjectStateId, ResourceId, SymbolId,
    UserPreferenceId, WorkItemId,
};

/// Mirrors `brainprint_engine::projection::planner::economy::ReuseIdentity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReuseIdentityWire {
    Resource(ResourceId),
    Symbol(SymbolId),
    Relation {
        kind: RelationKindWire,
        source: GraphEndpointWire,
        target: GraphEndpointWire,
    },
    Policy(PolicyId),
    Decision(DecisionId),
    Preference(UserPreferenceId),
    Blueprint(BlueprintApplicationId),
    ProjectState(ProjectStateId),
    WorkItem(WorkItemId),
    WorkingState(WorkItemId),
    WorkResult(WorkItemId),
    Handoff(WorkItemId),
    CurrentSource {
        resource: ResourceId,
        resource_revision: String,
        span: SourceSpanWire,
        role: RangeRoleWire,
    },
    RelatedTest {
        target: GraphEndpointWire,
        test: ResourceId,
    },
}

/// Mirrors `brainprint_engine::projection::planner::economy::ReuseReference`:
/// a Task 8 reference to a payload the caller's retained context already
/// acknowledged, delivered in the full item's place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReuseReferenceWire {
    pub identity: ReuseIdentityWire,
    /// SHA-256 of the full canonical payload, 64-char lowercase hex.
    pub version: String,
}

// ------------------------------------------------------------- delivery

/// Mirrors `brainprint_engine::projection::planner::economy::Measure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MeasureWire {
    Known(usize),
    Unknown,
    NotMeasured,
}

/// Mirrors `brainprint_engine::projection::planner::economy::StageAmount`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageAmountWire {
    pub items: MeasureWire,
    pub bytes: MeasureWire,
    pub tokens: MeasureWire,
}

/// Mirrors `brainprint_engine::projection::planner::delivery::DeliveryDimension`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryDimensionWire {
    Items,
    Bytes,
    Tokens,
}

/// Mirrors `ContinuationUnavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContinuationUnavailableWire {
    NoStableGeneration,
    UnitExceedsBudget,
    OptionalSourceTokenCostUnknown,
}

/// Delivery facts about one page, distilled from
/// `brainprint_engine::projection::planner::economy::ProjectionEconomy`
/// into the client-relevant subset: what was prepared vs. delivered, why
/// the page stopped, and whether a continuation exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryEconomyWire {
    pub raw_available: StageAmountWire,
    pub prepared: StageAmountWire,
    pub delivered: StageAmountWire,
    pub omitted_items: usize,
    pub more_available: bool,
    pub limiting: Vec<DeliveryDimensionWire>,
    pub continuation_unavailable: Option<ContinuationUnavailableWire>,
}

/// One delivered page: `brainprint_engine::projection::planner::economy::
/// PendingDelivery`'s `page`, restricted to what the client needs -- the
/// evidence/gaps plus the accounting `DeliveryEconomyWire` carries
/// separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryPageWire {
    pub evidence: Vec<EvidenceWire>,
    /// One per `evidence` item: `Some` when Task 8 delivered that item as
    /// a reference to an already-acknowledged identical payload instead
    /// of in full (#24 §8 "references").
    pub references: Vec<Option<ReuseReferenceWire>>,
    pub gaps: Vec<ProjectionGapWire>,
    pub used_items: usize,
    pub used_bytes: usize,
}

/// Mirrors `brainprint_engine::query_surface::ProjectedAnswer`, plus the
/// delivery/continuation/ack facts a client needs and Task 8 keeps behind
/// `PendingDelivery` engine-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedAnswerWire {
    pub target_resolution: TargetResolutionWire,
    pub currentness: CurrentnessWire,
    pub page: DeliveryPageWire,
    pub economy: DeliveryEconomyWire,
    pub continuation: Option<DeliveryContinuationWire>,
    pub more_available: bool,
}

// ------------------------------------------------------------------ find

/// Mirrors `brainprint_engine::query::FileListing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileListingWire {
    pub entries: Vec<ResourceWire>,
    pub truncated: bool,
    pub currentness: CurrentnessWire,
    pub source: ResultSourceWire,
}

/// Mirrors `brainprint_engine::search::QueryStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryStatusWire {
    Found,
    NotFound,
    Ambiguous,
    Unsupported,
    Truncated,
    Refreshing,
    Unavailable,
}

/// Mirrors `brainprint_engine::search::MatchSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchSourceWire {
    TextFallback,
}

/// Mirrors `brainprint_engine::search::FallbackReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FallbackReasonWire {
    ExplicitTextSearch,
    NonStructuralTarget,
    IncompleteStructuralCoverage,
}

/// Mirrors `brainprint_engine::search::BudgetAxis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetAxisWire {
    Results,
    Files,
    Bytes,
    Deadline,
}

/// Mirrors `brainprint_engine::search::ScopeReport`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeReportWire {
    pub files_scanned: usize,
    pub bytes_scanned: u64,
    pub binary_skipped: Vec<String>,
    pub oversized_skipped: Vec<String>,
    pub unreadable: Vec<String>,
    pub changed_during_scan: Vec<String>,
    pub budget_exhausted: Option<BudgetAxisWire>,
}

/// Mirrors `brainprint_engine::search::TextMatch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMatchWire {
    pub path_rel: String,
    pub resource_id: Option<ResourceId>,
    pub span: SourceSpanWire,
    pub preview: Option<String>,
    pub source: MatchSourceWire,
}

/// Mirrors `brainprint_engine::search::TextSearchResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextSearchResultWire {
    pub status: QueryStatusWire,
    pub matches: Vec<TextMatchWire>,
    pub scope: ScopeReportWire,
    pub structural_currentness: CurrentnessWire,
    pub reason: FallbackReasonWire,
}

/// Mirrors `brainprint_engine::query_surface::FindResult`.
// One per CLI/IPC call, never a hot loop; see `Request`'s
// `large_enum_variant` allow in `messages.rs`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindResultWire {
    Target(ProjectedAnswerWire),
    Files(FileListingWire),
    Text(TextSearchResultWire),
}

// -------------------------------------------------------------- relations

/// Mirrors `brainprint_engine::query_surface::RelationsResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationsResultWire {
    pub target: TargetResolutionWire,
    pub selection: Vec<EvidenceWire>,
    pub currentness: CurrentnessWire,
    pub answers: Vec<RelationAnswerWire>,
}

// -------------------------------------------------------------- structure

/// Mirrors `brainprint_engine::boundary::GroupBasis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupBasisWire {
    PathPrefix,
    DirectoryDepth,
    ResourceRole,
    ResourceLanguage,
    ResourceKind,
}

/// Mirrors `brainprint_engine::boundary::StructuralGroup`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StructuralGroupWire {
    Group(String),
    Ungrouped,
}

/// Mirrors `brainprint_engine::boundary::GroupCoverage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCoverageWire {
    pub supported: u64,
    pub partial: u64,
    pub unsupported: u64,
    pub structure_not_current: u64,
    pub relation_dirty: u64,
    pub unresolved_gaps: u64,
    pub candidate_gaps: u64,
    pub requires_semantics: u64,
    pub unsupported_construct: u64,
    pub candidate_truncated: u64,
    pub semantic_conflicts: u64,
    pub semantic_not_current: u64,
}

/// Mirrors `brainprint_engine::boundary::MemberSample`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberSampleWire {
    pub resource: ResourceId,
    pub path: String,
    pub role: ResourceRoleWire,
    pub language: Option<ResourceLanguageWire>,
    pub kind: ResourceKindWire,
}

/// Mirrors `brainprint_engine::boundary::GroupSummary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSummaryWire {
    pub group: StructuralGroupWire,
    pub prefixes: Vec<String>,
    pub resources: u64,
    pub internal_edges: u64,
    pub outgoing_edges: u64,
    pub incoming_edges: u64,
    pub fan_out_groups: usize,
    pub fan_in_groups: usize,
    pub coverage: GroupCoverageWire,
    pub members: Vec<MemberSampleWire>,
}

/// Mirrors `brainprint_engine::boundary::BoundaryEdge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryEdgeWire {
    pub source: StructuralGroupWire,
    pub target: StructuralGroupWire,
    pub kind: RelationKindWire,
    pub confirmed_edges: u64,
}

/// Mirrors `brainprint_engine::boundary::GapAggregate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapAggregateWire {
    pub group: StructuralGroupWire,
    pub intended: super::relations::IntendedRelationWire,
    pub reason: super::relations::UnresolvedReasonWire,
    pub unresolved: u64,
    pub candidate: u64,
    pub candidate_truncated: u64,
}

/// Mirrors `brainprint_engine::boundary::StructuralSummary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuralSummaryWire {
    pub basis: GroupBasisWire,
    pub relation_kinds: Vec<RelationKindWire>,
    pub currentness: CurrentnessWire,
    pub groups: Vec<GroupSummaryWire>,
    pub boundary_edges: Vec<BoundaryEdgeWire>,
    pub cycles: Option<Vec<Vec<StructuralGroupWire>>>,
    pub gaps: Vec<GapAggregateWire>,
}

// -------------------------------------------------------------- envelope

/// Mirrors `brainprint_engine::query_surface::CoreQuerySurface`'s seven
/// operation results (#24 §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryResultWire {
    Find(FindResultWire),
    Inspect(ProjectedAnswerWire),
    Relations(RelationsResultWire),
    Impact(ProjectedAnswerWire),
    Context(ProjectedAnswerWire),
    Knowledge(KnowledgeResultWire),
    Structure(StructuralSummaryWire),
}
