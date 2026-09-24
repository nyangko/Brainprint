//! Wire mirror of the seven Task 10 operations and their request shapes
//! (#24 §4, §11, §12).

use serde::{Deserialize, Serialize};

use super::{
    delivery::DeliveryWire,
    evidence::ChangeKindWire,
    knowledge::{KnowledgeScopeWire, RequestDirectiveWire, WorkItemStatusWire},
    target::{
        ProjectionTargetWire, RelationKindWire, ResourceKindWire, ResourceLanguageWire,
        ResourceRoleWire,
    },
};
use crate::{BlueprintApplicationId, DecisionId, PolicyId, WorkItemId};

/// Mirrors the caller-supplied knowledge subjects of
/// `brainprint_engine::projection::ProjectionKnowledgeRefs`. Sets on the
/// engine side; plain lists on the wire (duplicates/order do not matter
/// -- the adapter always reconstructs a set).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionKnowledgeRefsWire {
    #[serde(default)]
    pub decision_topics: Vec<String>,
    #[serde(default)]
    pub preference_keys: Vec<String>,
    #[serde(default)]
    pub state_keys: Vec<String>,
    #[serde(default)]
    pub blueprint_applications: Vec<BlueprintApplicationId>,
}

/// Mirrors `brainprint_engine::search::TextPattern`, owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextPatternWire {
    Literal(String),
    Regex(String),
}

/// Mirrors `brainprint_engine::search::SearchBudget`. `deadline_ms` is the
/// wall-clock ceiling in milliseconds; `None` means no deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchBudgetWire {
    pub max_results: usize,
    pub max_files: usize,
    pub max_bytes: u64,
    pub deadline_ms: Option<u64>,
}

/// Mirrors `brainprint_engine::query_surface::FindQuery`.
// One per CLI/IPC call, never a hot loop; see `Request`'s
// `large_enum_variant` allow in `messages.rs`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindQueryWire {
    Target {
        target: ProjectionTargetWire,
        delivery: DeliveryWire,
    },
    Files {
        directory: Option<String>,
        recursive: bool,
        path_prefix: Option<String>,
        role: Option<ResourceRoleWire>,
        language: Option<ResourceLanguageWire>,
        kind: Option<ResourceKindWire>,
        limit: std::num::NonZeroUsize,
    },
    Text {
        pattern: TextPatternWire,
        case_insensitive: bool,
        path_prefix: Option<String>,
        search_budget: SearchBudgetWire,
        max_file_bytes: u64,
        with_preview: bool,
    },
}

/// Mirrors `brainprint_engine::query_surface::InspectRequest`'s
/// operation-specific fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectWire {
    pub target: ProjectionTargetWire,
    pub delivery: DeliveryWire,
}

/// Mirrors `brainprint_engine::query_surface::RelationDirection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationDirectionWire {
    Outgoing,
    Incoming,
    Both,
}

/// Mirrors `brainprint_engine::query_surface::RelationsRequest`'s
/// operation-specific fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationsWire {
    pub target: ProjectionTargetWire,
    pub direction: RelationDirectionWire,
    pub kinds: Vec<RelationKindWire>,
}

/// Mirrors `brainprint_engine::query_surface::ImpactRequest`'s
/// operation-specific fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactWire {
    pub target: ProjectionTargetWire,
    pub change: ChangeKindWire,
    pub delivery: DeliveryWire,
}

/// Mirrors `brainprint_engine::query_surface::ContextPurpose`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextWire {
    Change {
        target: ProjectionTargetWire,
        change: Option<ChangeKindWire>,
        work_item: Option<WorkItemId>,
        scope_layers: Vec<Vec<KnowledgeScopeWire>>,
        directives: Vec<RequestDirectiveWire>,
        knowledge_refs: ProjectionKnowledgeRefsWire,
        delivery: DeliveryWire,
    },
    Resume {
        work_item: WorkItemId,
        target: Option<ProjectionTargetWire>,
        scope_layers: Vec<Vec<KnowledgeScopeWire>>,
        directives: Vec<RequestDirectiveWire>,
        knowledge_refs: ProjectionKnowledgeRefsWire,
        delivery: DeliveryWire,
    },
}

/// Mirrors `brainprint_engine::query_surface::LineageTarget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineageTargetWire {
    ProjectPolicy(PolicyId),
    UserPolicy(PolicyId),
    Decision(DecisionId),
}

/// Mirrors `brainprint_engine::query_surface::KnowledgeQuery`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnowledgeWire {
    Rules {
        scope_layers: Vec<Vec<KnowledgeScopeWire>>,
        directives: Vec<RequestDirectiveWire>,
        knowledge_refs: ProjectionKnowledgeRefsWire,
    },
    WorkItems {
        statuses: Vec<WorkItemStatusWire>,
        limit: std::num::NonZeroUsize,
    },
    Lineage(LineageTargetWire),
    Handoffs {
        work_item: WorkItemId,
        limit: std::num::NonZeroUsize,
    },
}

/// Mirrors `brainprint_engine::boundary::GroupingSpec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupingSpecWire {
    PathPrefixes(Vec<PathGroupRuleWire>),
    DirectoryDepth { root: String, depth: usize },
    ResourceRole,
    ResourceLanguage,
    ResourceKind,
}

/// Mirrors `brainprint_engine::boundary::PathGroupRule`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathGroupRuleWire {
    pub label: String,
    pub prefix: String,
}

/// Mirrors `brainprint_engine::boundary::ResourceScope` (the summary
/// scope, distinct from a Symbol-search resource scope).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryResourceScopeWire {
    pub path_prefix: Option<String>,
    pub role: Option<ResourceRoleWire>,
    pub language: Option<ResourceLanguageWire>,
    pub kind: Option<ResourceKindWire>,
}

/// Mirrors `brainprint_engine::boundary::StructuralSummaryRequest`'s
/// operation-specific fields (the `workspace` field is carried by the
/// envelope's `WorkspaceSelectorWire` instead).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructureWire {
    pub grouping: GroupingSpecWire,
    pub resource_scope: SummaryResourceScopeWire,
    pub relation_kinds: Vec<RelationKindWire>,
    pub include_ungrouped: bool,
    pub include_cycles: bool,
    pub member_sample_limit: Option<std::num::NonZeroUsize>,
}

/// Mirrors `brainprint_engine::query_surface::CoreQuerySurface`'s seven
/// operations (#24 §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryOperationWire {
    Find(FindQueryWire),
    Inspect(InspectWire),
    Relations(RelationsWire),
    Impact(ImpactWire),
    Context(ContextWire),
    Knowledge(KnowledgeWire),
    Structure(StructureWire),
}
