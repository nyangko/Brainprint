//! Wire mirror of the Task 10 `relations` result family (#24 §8).

use serde::{Deserialize, Serialize};

use super::{
    common::{DispatchWire, EvidenceLocationWire, FreshnessWire, SupportWire, TargetScopeWire},
    target::{GraphEndpointWire, RelationKindWire},
};

/// Mirrors `brainprint_engine::relations::Direction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectionWire {
    Outgoing,
    Incoming,
}

/// Mirrors `brainprint_engine::resolution::Resolution`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionWire {
    Resolved,
    Candidate,
    Unresolved,
}

/// Mirrors `brainprint_engine::relations::RelationResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationResultWire {
    pub kind: RelationKindWire,
    pub source: GraphEndpointWire,
    pub target: GraphEndpointWire,
    pub direction: DirectionWire,
    pub dispatch: DispatchWire,
    pub target_scope: TargetScopeWire,
    pub resolution: ResolutionWire,
    pub support: SupportWire,
    pub freshness: FreshnessWire,
    pub evidence: Vec<EvidenceLocationWire>,
}

/// Mirrors `brainprint_engine::gaps::IntendedRelation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntendedRelationWire {
    Known(RelationKindWire),
    Inheritance,
}

/// Mirrors `brainprint_engine::gaps::UnresolvedReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnresolvedReasonWire {
    MissingRelativeTarget,
    ModuleTreeRequiresSemantics,
    NamespaceRequiresSemantics,
    ConfigDependentSpecifier,
    CompoundSpecifier,
    AmbiguousCandidates,
    PossiblyShadowed,
    ReceiverTypeRequired,
    NoStructuralBinding,
    ImportTargetUnresolved,
    NameNotInModule,
    NotANameExpression,
    TypeSemanticsRequired,
    RelationKindNotStructural,
    OverrideTargetRequiresSemantics,
    DynamicKeyExpression,
}

/// Mirrors `brainprint_engine::relations::RelationGap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationGapWire {
    pub location: EvidenceLocationWire,
    pub intended: IntendedRelationWire,
    pub lookup_name: String,
    pub module_hint: Option<String>,
    pub reason: UnresolvedReasonWire,
    pub resolution: ResolutionWire,
    pub candidates: Vec<GraphEndpointWire>,
    pub candidate_truncated: bool,
    pub resolution_context_key: Option<String>,
}

/// Mirrors `brainprint_engine::relations::GapAttribution`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GapAttributionWire {
    SourceScoped,
    TargetCandidateOnly,
}

/// Mirrors `brainprint_engine::merge::SemanticScope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticScopeWire {
    pub contexts: usize,
    pub conflicts: usize,
    pub not_current: bool,
}

/// Mirrors `brainprint_engine::relations::ScopeState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeStateWire {
    pub resource: crate::ResourceId,
    pub support: SupportWire,
    pub freshness: FreshnessWire,
}

/// Mirrors `brainprint_engine::relations::Coverage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageWire {
    pub attribution: GapAttributionWire,
    pub gaps: usize,
    pub ambiguous: usize,
    pub requires_semantics: usize,
    pub unsupported_construct: usize,
    pub truncated: usize,
    pub unattributed: usize,
    pub scope: Option<ScopeStateWire>,
    pub semantic: SemanticScopeWire,
}

/// Mirrors `brainprint_engine::relations::RelationAnswer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationAnswerWire {
    pub direction: DirectionWire,
    pub kinds: Vec<RelationKindWire>,
    pub confirmed: Vec<RelationResultWire>,
    pub gaps: Vec<RelationGapWire>,
    pub coverage: CoverageWire,
}

/// Mirrors `brainprint_engine::related_tests::ProjectionBasis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectionBasisWire {
    DirectRelation(RelationKindWire),
    RelationPath {
        hops: usize,
        first: RelationKindWire,
    },
}

/// Mirrors `brainprint_engine::related_tests::TestPath`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestPathWire {
    pub hops: Vec<RelationResultWire>,
}

/// Mirrors `brainprint_engine::related_tests::RelatedTestCandidate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedTestCandidateWire {
    pub resource: crate::ResourceId,
    pub path_rel: String,
    pub endpoints: Vec<GraphEndpointWire>,
    pub distance: usize,
    pub basis: ProjectionBasisWire,
    pub paths: Vec<TestPathWire>,
    pub paths_truncated: bool,
    pub support: SupportWire,
    pub freshness: FreshnessWire,
}
