//! Wire mirror of the Task 1/2/3/4 knowledge domain (#24 §8).

use serde::{Deserialize, Serialize};

use crate::{
    BlueprintApplicationId, BlueprintId, DecisionId, IndexIncarnationId, PolicyId, ProjectStateId,
    ResourceId, UserPreferenceId, WorkItemId,
};

/// Mirrors `brainprint_engine::knowledge::ScopeKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeKindWire {
    Global,
    Project,
    Workspace,
    Package,
    Module,
    Directory,
    Resource,
    Domain,
    Task,
}

/// Mirrors `brainprint_engine::knowledge::KnowledgeScope`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeScopeWire {
    pub kind: ScopeKindWire,
    pub key: Option<String>,
}

/// Mirrors `brainprint_engine::knowledge::SourceKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKindWire {
    UserExplicit,
    AuthoritativeArtifact,
    Observed,
    AgentReported,
}

/// Mirrors `brainprint_engine::knowledge::Provenance`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceWire {
    pub source_kind: SourceKindWire,
    pub locator: Option<String>,
    pub revision: Option<String>,
}

/// Mirrors `brainprint_engine::knowledge::TypedValue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypedValueWire {
    Text(String),
    Integer(i64),
    Boolean(bool),
    Json(serde_json::Value),
}

// ------------------------------------------------------------- directive

/// Mirrors `brainprint_engine::knowledge::DirectiveTarget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectiveTargetWire {
    Policy,
    Decision,
    Preference,
}

/// Mirrors `brainprint_engine::knowledge::RequestDirective`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestDirectiveWire {
    pub id: String,
    pub target: DirectiveTargetWire,
    pub subject_key: String,
    pub scope: KnowledgeScopeWire,
    pub summary: String,
}

// --------------------------------------------------------------- policy

/// Mirrors `brainprint_engine::knowledge::ProtectionClass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtectionClassWire {
    Normal,
    ProtectedPrivacy,
    ProtectedSecurity,
}

/// Mirrors `brainprint_engine::knowledge::PriorityClass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PriorityClassWire {
    Default,
}

/// Mirrors `brainprint_engine::knowledge::PolicyStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyStatusWire {
    Active,
    Superseded,
    Disabled,
}

/// Mirrors `brainprint_engine::knowledge::Policy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyWire {
    pub uid: PolicyId,
    pub scope: KnowledgeScopeWire,
    pub policy_key: Option<String>,
    pub title: String,
    pub rule_text: String,
    pub structured_rule: Option<serde_json::Value>,
    pub protection_class: ProtectionClassWire,
    pub priority_class: PriorityClassWire,
    pub status: PolicyStatusWire,
    pub provenance: ProvenanceWire,
    pub created_at: String,
    pub updated_at: String,
}

/// Mirrors `brainprint_engine::knowledge::PolicyLineage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyLineageWire {
    pub supersedes: Vec<PolicyId>,
    pub superseded_by: Vec<PolicyId>,
}

// ------------------------------------------------------------- decision

/// Mirrors `brainprint_engine::knowledge::DecisionStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionStatusWire {
    Active,
    Superseded,
    Reversed,
}

/// Mirrors `brainprint_engine::knowledge::Decision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionWire {
    pub uid: DecisionId,
    pub scope: KnowledgeScopeWire,
    pub topic: String,
    pub chosen_summary: String,
    pub rationale: String,
    pub status: DecisionStatusWire,
    pub provenance: ProvenanceWire,
    pub created_at: String,
    pub updated_at: String,
}

/// Mirrors `brainprint_engine::knowledge::DecisionLinkKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionLinkKindWire {
    Supersedes,
    Reverses,
}

/// Mirrors `brainprint_engine::knowledge::DecisionLink`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionLinkWire {
    pub kind: DecisionLinkKindWire,
    pub other: DecisionId,
}

/// Mirrors `brainprint_engine::knowledge::DecisionLineage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionLineageWire {
    pub outgoing: Vec<DecisionLinkWire>,
    pub incoming: Vec<DecisionLinkWire>,
}

// ----------------------------------------------------------- preference

/// Mirrors `brainprint_engine::knowledge::PreferenceStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreferenceStatusWire {
    Active,
    Superseded,
    Disabled,
}

/// Mirrors `brainprint_engine::knowledge::UserPreference`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPreferenceWire {
    pub uid: UserPreferenceId,
    pub scope: KnowledgeScopeWire,
    pub preference_key: String,
    pub value: TypedValueWire,
    pub status: PreferenceStatusWire,
    pub provenance: ProvenanceWire,
    pub created_at: String,
    pub updated_at: String,
}

// --------------------------------------------------------- project state

/// Mirrors `brainprint_engine::knowledge::ProjectStateStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectStateStatusWire {
    Current,
    Retired,
}

/// Mirrors `brainprint_engine::knowledge::ProjectState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStateWire {
    pub uid: ProjectStateId,
    pub key: String,
    pub scope: KnowledgeScopeWire,
    pub value: TypedValueWire,
    pub status: ProjectStateStatusWire,
    pub provenance: ProvenanceWire,
    pub updated_at: String,
}

// ----------------------------------------------------------- blueprint

/// Mirrors `brainprint_engine::knowledge::BlueprintStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlueprintStatusWire {
    Draft,
    Active,
    Retired,
}

/// Mirrors `brainprint_engine::knowledge::BlueprintOwnerKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlueprintOwnerKindWire {
    Global,
    Project,
}

/// Mirrors `brainprint_engine::knowledge::BlueprintRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintRefWire {
    pub owner: BlueprintOwnerKindWire,
    pub uid: BlueprintId,
}

/// Mirrors `brainprint_engine::knowledge::BlueprintComponent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintComponentWire {
    pub name: String,
    pub description: Option<String>,
}

/// Mirrors `brainprint_engine::knowledge::BlueprintRelationship`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintRelationshipWire {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub description: Option<String>,
}

/// Mirrors `brainprint_engine::knowledge::BlueprintDefinition`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlueprintDefinitionWire {
    pub components: Vec<BlueprintComponentWire>,
    pub relationships: Vec<BlueprintRelationshipWire>,
    pub constraints: Vec<String>,
}

/// Mirrors `brainprint_engine::knowledge::Blueprint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintWire {
    pub uid: BlueprintId,
    pub scope: KnowledgeScopeWire,
    pub blueprint_key: Option<String>,
    pub title: String,
    pub intent: String,
    pub definition: BlueprintDefinitionWire,
    pub status: BlueprintStatusWire,
    pub version: Option<String>,
    pub provenance: ProvenanceWire,
    pub created_at: String,
    pub updated_at: String,
}

/// Mirrors `brainprint_engine::knowledge::BlueprintDefinitionState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlueprintDefinitionStateWire {
    Available(Box<BlueprintWire>),
    Missing,
    Retired,
}

/// Mirrors `brainprint_engine::knowledge::BlueprintApplicationStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlueprintApplicationStatusWire {
    Active,
    Retired,
}

/// Mirrors `brainprint_engine::knowledge::BlueprintApplication`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintApplicationWire {
    pub uid: BlueprintApplicationId,
    pub blueprint: BlueprintRefWire,
    pub scope: KnowledgeScopeWire,
    pub status: BlueprintApplicationStatusWire,
    pub application_summary: String,
    pub provenance: ProvenanceWire,
    pub created_at: String,
    pub updated_at: String,
}

/// Mirrors `brainprint_engine::knowledge::BlueprintEvidence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintEvidenceWire {
    pub application: BlueprintApplicationWire,
    pub definition: BlueprintDefinitionStateWire,
}

// -------------------------------------------------------------- resolved

/// Mirrors `brainprint_engine::knowledge::Origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OriginWire {
    Request,
    Global,
    Project,
    Workspace,
}

/// Mirrors `brainprint_engine::knowledge::ResolutionReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionReasonWire {
    ProtectedConstraint,
    RequestExplicit,
    UnkeyedPolicy,
    SelectedPolicy,
    ResolvedDecision,
    AppliedPreference,
    BlueprintEvidence,
    StateEvidence,
    ShadowedByRequest,
    ShadowedByProjectTier,
    ShadowedByMoreSpecificScope,
    ShadowedByProjectDecision,
}

/// Mirrors `brainprint_engine::knowledge::Resolved<T>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedWire<T> {
    pub item: T,
    pub origin: OriginWire,
    pub layer: usize,
    pub reason: ResolutionReasonWire,
}

/// Mirrors `brainprint_engine::knowledge::EvidenceCategory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceCategoryWire {
    Policy,
    Decision,
    Preference,
    Directive,
}

/// Mirrors `brainprint_engine::knowledge::EvidenceRef`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRefWire {
    pub category: EvidenceCategoryWire,
    pub id: String,
    pub origin: OriginWire,
    pub scope: KnowledgeScopeWire,
    pub layer: usize,
    pub source_kind: Option<SourceKindWire>,
    pub status: Option<String>,
}

/// Mirrors `brainprint_engine::knowledge::ConflictKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictKindWire {
    SameSpecificityDecision,
    SameSpecificityPreference,
    ProtectedOverrideRejected,
    InvalidProtectedProvenance,
}

/// Mirrors `brainprint_engine::knowledge::KnowledgeConflict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeConflictWire {
    pub kind: ConflictKindWire,
    pub subject: Option<String>,
    pub involved: Vec<EvidenceRefWire>,
}

// ------------------------------------------------------------- work item

/// Mirrors `brainprint_engine::knowledge::WorkItemSourceKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkItemSourceKindWire {
    Issue,
    ExternalTask,
    UserRequest,
}

/// Mirrors `brainprint_engine::knowledge::WorkItemStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkItemStatusWire {
    Open,
    Active,
    Blocked,
    Paused,
    Completed,
    Abandoned,
}

/// Mirrors `brainprint_engine::knowledge::WorkItem`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItemWire {
    pub uid: WorkItemId,
    pub source_kind: WorkItemSourceKindWire,
    pub source_ref: Option<String>,
    pub title: Option<String>,
    pub goal: String,
    pub status: WorkItemStatusWire,
    pub created_at: String,
    pub closed_at: Option<String>,
}

/// Mirrors `brainprint_engine::knowledge::WorkHandoff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkHandoffWire {
    pub work_item: WorkItemId,
    pub handoff_summary: String,
    pub remaining_summary: Option<String>,
    pub blocker_summary: Option<String>,
    pub next_scope_hint: Option<String>,
    pub created_at: String,
}

/// Mirrors `brainprint_engine::knowledge::WorkResultStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkResultStatusWire {
    Completed,
    Partial,
    Abandoned,
}

/// Mirrors `brainprint_engine::knowledge::DirtyObservation`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirtyObservationWire {
    Unknown,
    Clean,
    Dirty { fingerprint: String },
}

/// Mirrors `brainprint_engine::knowledge::WorkResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkResultWire {
    pub work_item: WorkItemId,
    pub result_status: WorkResultStatusWire,
    pub result_summary: String,
    pub commit_id: Option<String>,
    pub change_set_fingerprint: Option<String>,
    pub verification_summary: Option<String>,
    pub result_workspace_revision: String,
    pub result_index_incarnation: Option<IndexIncarnationId>,
    pub result_generation_no: Option<i64>,
    pub remaining_dirty: DirtyObservationWire,
    pub created_at: String,
}

/// Mirrors `brainprint_engine::knowledge::WorkingState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingStateWire {
    pub work_item: WorkItemId,
    pub baseline_workspace_revision: String,
    pub baseline_index_incarnation: Option<IndexIncarnationId>,
    pub baseline_generation_no: i64,
    pub baseline_head: Option<String>,
    pub baseline_dirty: DirtyObservationWire,
    pub current_step: Option<String>,
    pub progress_summary: Option<String>,
    pub remaining_summary: Option<String>,
    pub blocker_summary: Option<String>,
    pub owner_agent: Option<String>,
    pub last_observed_workspace_revision: String,
    pub updated_at: String,
}

/// Mirrors `brainprint_engine::knowledge::WorkResourceRole`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkResourceRoleWire {
    Target,
    Touched,
    Owned,
    Related,
    PreexistingDirty,
}

/// Mirrors `brainprint_engine::knowledge::WorkOverlap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkOverlapWire {
    pub other: WorkItemId,
    pub other_status: WorkItemStatusWire,
    pub resource: ResourceId,
    pub this_roles: Vec<WorkResourceRoleWire>,
    pub other_roles: Vec<WorkResourceRoleWire>,
}

/// Mirrors `brainprint_engine::knowledge::GenerationReferenceState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerationReferenceStateWire {
    PresentMatching,
    HistoricalMissingOrReused,
}

/// Mirrors `brainprint_engine::knowledge::GenerationReference`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationReferenceWire {
    pub index_incarnation: Option<IndexIncarnationId>,
    pub generation_no: i64,
    pub workspace_revision: String,
    pub state: GenerationReferenceStateWire,
}

/// Mirrors `brainprint_engine::knowledge::Staleness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StalenessWire {
    NotEvaluated,
    CurrentAtCutoff,
    PossiblyStale,
    Closed,
}

/// Mirrors `brainprint_engine::query_surface::KnowledgeResult`'s
/// `Rules`/`WorkItems`/`Handoffs` outcomes, and the lineage variants below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnowledgeResultWire {
    Rules {
        evidence: Vec<super::evidence::EvidenceWire>,
        gaps: Vec<super::evidence::ProjectionGapWire>,
    },
    WorkItems {
        items: Vec<WorkItemWire>,
        truncated: bool,
    },
    PolicyLineage(PolicyLineageWire),
    DecisionLineage(DecisionLineageWire),
    Handoffs {
        work_item: WorkItemId,
        handoffs: Vec<WorkHandoffWire>,
        truncated: bool,
    },
}
