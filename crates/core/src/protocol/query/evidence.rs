//! Wire mirror of `brainprint_engine::projection::EvidenceItem` and its
//! nested payloads, variant-for-variant (#24 §8).

use serde::{Deserialize, Serialize};

use super::{
    common::{
        CoverageNoteWire, CurrentnessWire, ResourceWire, SourceSpanWire, StructuralCoverageWire,
        SymbolCandidateWire, line_1based,
    },
    knowledge::{
        BlueprintEvidenceWire, DecisionWire, KnowledgeConflictWire, PolicyWire, ProjectStateWire,
        RequestDirectiveWire, StalenessWire, UserPreferenceWire, WorkHandoffWire, WorkItemWire,
        WorkOverlapWire, WorkResultWire, WorkingStateWire,
    },
    relations::{RelatedTestCandidateWire, RelationGapWire, RelationResultWire},
    target::{GraphEndpointWire, ProjectionTargetWire, RelationKindWire, SymbolKindWire},
};
use crate::{ResourceId, SymbolId, WorkItemId, WorkspaceId};

/// Mirrors `brainprint_engine::projection::ResourceOutline` (#58).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceOutlineWire {
    pub resource: ResourceId,
    pub path_rel: String,
    pub resource_revision: String,
    pub coverage: StructuralCoverageWire,
    pub entries: Vec<OutlineEntryWire>,
}

/// Mirrors `brainprint_engine::projection::OutlineEntry`; lines are the
/// span's own 0-based lines, serialized with derived `start_line_1based` /
/// `end_line_1based` editor lines (#76, see `SourcePointWire`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "OutlineEntryOut")]
pub struct OutlineEntryWire {
    pub symbol: SymbolId,
    pub parent: Option<SymbolId>,
    pub kind: SymbolKindWire,
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Serialize)]
struct OutlineEntryOut {
    symbol: SymbolId,
    parent: Option<SymbolId>,
    kind: SymbolKindWire,
    name: String,
    start_line: usize,
    end_line: usize,
    start_line_1based: usize,
    end_line_1based: usize,
}

impl From<OutlineEntryWire> for OutlineEntryOut {
    fn from(entry: OutlineEntryWire) -> Self {
        Self {
            start_line_1based: line_1based(entry.start_line),
            end_line_1based: line_1based(entry.end_line),
            symbol: entry.symbol,
            parent: entry.parent,
            kind: entry.kind,
            name: entry.name,
            start_line: entry.start_line,
            end_line: entry.end_line,
        }
    }
}

/// Mirrors `brainprint_engine::impact::ImpactIntent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImpactIntentWire {
    PublicSignatureChange,
    Rename,
    ModuleMove,
    BaseInterfaceChange,
}

/// Mirrors `brainprint_engine::projection::ChangeKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKindWire {
    Structural(ImpactIntentWire),
    Delete,
    DomainContractChange,
}

/// Mirrors `brainprint_engine::projection::GenerationBasis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerationBasisWire {
    Baseline,
    Result,
}

/// Mirrors `brainprint_engine::query_surface::TargetResolution`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetResolutionWire {
    Resolved(GraphEndpointWire),
    MultipleCandidates,
    SingleNonExactCandidate,
    NotFound,
    NotFoundIncompleteCoverage,
    NotCurrent,
    NoTarget,
}

/// Mirrors `brainprint_engine::query::Located`'s `exact_selector`; used
/// only inside `TargetSelectionWire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSelectionWire {
    pub selector: ProjectionTargetWire,
    pub candidates: Vec<GraphEndpointWire>,
    pub last_valid: Vec<GraphEndpointWire>,
    pub exact_selector: bool,
    pub truncated: bool,
    pub currentness: CurrentnessWire,
    pub source: super::common::ResultSourceWire,
    pub incomplete_coverage: Vec<CoverageNoteWire>,
}

/// Mirrors `brainprint_engine::coverage::CoverageLimit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoverageLimitWire {
    UnresolvedEvidence,
    AmbiguousCandidates,
    RequiresSemantics,
    UnsupportedConstruct,
    CandidateTruncated,
    UnattributedGaps,
    ReverseScopeNotEnumerable,
    PartialSupport,
    UnsupportedScope,
    StaleEvidence,
    DirtyRelationComponent,
    TraversalTruncated,
    SupportingPathTruncated,
    UnknownResourceRole,
    UnreadableResourceOwner,
    IndexNotCurrent,
    SemanticConflict,
    SemanticNotCurrent,
    UnconfirmedCallerOwners,
}

/// Mirrors `brainprint_engine::coverage::CoverageReport`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CoverageReportWire {
    pub limits: Vec<CoverageLimitWire>,
}

/// Mirrors `brainprint_engine::coverage::AnswerState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnswerStateWire {
    Confirmed,
    NoneUnderCompleteCoverage,
    NoneWithIncompleteCoverage,
}

/// Mirrors `brainprint_engine::relations::Direction`, reused here for
/// `CoverageSubjectWire::Relations`.
pub use super::relations::DirectionWire;

/// Mirrors `brainprint_engine::projection::CoverageSubject`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoverageSubjectWire {
    TargetSelection(ProjectionTargetWire),
    Relations {
        anchor: GraphEndpointWire,
        direction: DirectionWire,
        kinds: Vec<RelationKindWire>,
    },
    Impact {
        root: GraphEndpointWire,
        intent: ImpactIntentWire,
    },
    RelatedTests {
        target: GraphEndpointWire,
        intent: ImpactIntentWire,
    },
}

/// Mirrors `brainprint_engine::projection::CoverageEvidence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageEvidenceWire {
    pub subject: CoverageSubjectWire,
    pub report: CoverageReportWire,
    pub confirmed: usize,
    pub answer_state: AnswerStateWire,
}

/// Mirrors `brainprint_engine::prepare::SourceUnavailable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceUnavailableWire {
    StaleBasis {
        basis_revision: String,
        current_revision: String,
    },
    SourceChanged {
        expected_content_hash: String,
        observed_content_hash: String,
    },
    SymbolNotCurrent {
        symbol_revision: String,
        resource_revision: String,
    },
    NoCurrentSource {
        detail: String,
    },
    SpanNotReadable {
        detail: String,
    },
}

/// Mirrors `brainprint_engine::prepare::RangeRole`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RangeRoleWire {
    EvidenceSpan,
    ContainingDeclaration,
    AnchorDeclaration,
    CandidateDeclaration,
    MemberDeclaration,
}

/// Mirrors `brainprint_engine::inspect::SourceVerification`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceVerificationWire {
    pub expected_content_hash: String,
    pub observed_content_hash: String,
    pub currentness: CurrentnessWire,
}

/// Mirrors `brainprint_engine::prepare::PreparedRange`: a verified current
/// source range (`EvidenceItem::CurrentSource`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedRangeWire {
    pub resource: ResourceId,
    pub path_rel: String,
    pub resource_revision: String,
    pub span: SourceSpanWire,
    pub source: String,
    pub role: RangeRoleWire,
    pub verification: SourceVerificationWire,
}

/// Mirrors `brainprint_engine::projection::planner::ProjectionGap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectionGapWire {
    TargetAmbiguous,
    TargetNotExact,
    TargetNotFound,
    TargetNotFoundWithIncompleteCoverage,
    TargetNotCurrent,
    UnsupportedImpactProfile(ChangeKindWire),
    DependencyExpansionUndefined,
    BlueprintApplicationNotApplied(crate::BlueprintApplicationId),
    RequiresSemantics,
    NotCurrent,
    /// Resources still hold an unresolved use site with the target's
    /// name that no semantic refresh confirmed (a count, never a claim).
    UnconfirmedCallerOwners(usize),
}

/// Mirrors `brainprint_engine::projection::ProjectedRelation` (#73): the
/// relation's own fields, then `sites[i]` for `evidence[i]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedRelationWire {
    #[serde(flatten)]
    pub relation: RelationResultWire,
    pub sites: Vec<EvidenceSiteWire>,
}

/// Mirrors `brainprint_engine::projection::EvidenceSite`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSiteWire {
    pub path_rel: Option<String>,
    /// `span.start.line + 1` of the matching evidence span.
    pub line_1based: usize,
    pub owner: Option<SiteOwnerWire>,
}

/// Mirrors `brainprint_engine::projection::SiteOwner`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteOwnerWire {
    pub qualified_name: String,
    pub kind: SymbolKindWire,
}

/// Mirrors `brainprint_engine::projection::EvidenceItem`, variant-for-
/// variant. Nested wire records preserve every public field required to
/// reconstruct the public Task 10 result; the adapter adds no payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceWire {
    // STORED
    Resource(ResourceWire),
    Symbol(SymbolCandidateWire),
    Relation(ProjectedRelationWire),
    RelationGap(RelationGapWire),
    Policy(ResolvedPolicyWire),
    Decision(ResolvedDecisionWire),
    Preference(ResolvedPreferenceWire),
    Blueprint(ResolvedBlueprintWire),
    ProjectState(ResolvedProjectStateWire),
    WorkItem(WorkItemWire),
    WorkingState(WorkingStateWire),
    WorkResult(WorkResultWire),
    Handoff(WorkHandoffWire),

    // OBSERVED
    CurrentSource(PreparedRangeWire),
    Directive(ResolvedDirectiveWire),

    // DERIVED
    RelatedTest {
        target: GraphEndpointWire,
        candidate: RelatedTestCandidateWire,
    },
    KnowledgeConflict(KnowledgeConflictWire),
    WorkOverlap {
        work_item: WorkItemId,
        overlap: WorkOverlapWire,
    },
    GenerationReference {
        work_item: WorkItemId,
        basis: GenerationBasisWire,
        reference: super::knowledge::GenerationReferenceWire,
    },
    WorkStaleness {
        work_item: WorkItemId,
        staleness: StalenessWire,
    },
    TargetSelection(TargetSelectionWire),
    /// #58: an exactly selected Resource's outline.
    Outline(ResourceOutlineWire),
    Coverage(CoverageEvidenceWire),
    SourceUnavailable {
        resource: ResourceId,
        span: SourceSpanWire,
        reason: SourceUnavailableWire,
    },
    IndexCurrentness {
        workspace: WorkspaceId,
        currentness: CurrentnessWire,
    },
}

/// `Resolved<Policy>`, spelled out because generic wire types over a
/// closed set of payloads are clearer than a shared `ResolvedWire<T>`
/// instantiation at every call site.
pub type ResolvedPolicyWire = super::knowledge::ResolvedWire<PolicyWire>;
pub type ResolvedDecisionWire = super::knowledge::ResolvedWire<DecisionWire>;
pub type ResolvedPreferenceWire = super::knowledge::ResolvedWire<UserPreferenceWire>;
pub type ResolvedBlueprintWire = super::knowledge::ResolvedWire<BlueprintEvidenceWire>;
pub type ResolvedProjectStateWire = super::knowledge::ResolvedWire<ProjectStateWire>;
pub type ResolvedDirectiveWire = super::knowledge::ResolvedWire<RequestDirectiveWire>;
