//! Typed I5 Project Intelligence model (#20 task 1).
//!
//! The five categories stay distinct types: Policy, Decision, Blueprint,
//! Project State, Working State (#7 task 1). There is no universal
//! `KnowledgeItem { kind, json }` and no credential/secret category; the
//! only intentionally generic value is [`TypedValue`], whose declared type
//! is checked against its JSON at every boundary.
//!
//! Task 1 stores scope, status, provenance, lineage, protection class and
//! priority class. It does not decide which item wins (task 2), who may
//! promote what (task 4), or what an Agent receives (task 5+).

use std::{collections::BTreeSet, fmt};

use brainprint_core::{
    BlueprintApplicationId, BlueprintId, DecisionId, PolicyId, ProjectStateId, ResourceId,
    UserPreferenceId, WorkItemId, WorkNoteId, WorkspaceId,
};
use serde::{Deserialize, Serialize};

use crate::resolution::{UnknownAxisValue, closed_vocabulary};

use super::KnowledgeError;

// ---------------------------------------------------------------- scope

/// Declared applicability axis (#7 task 2 §4). Task 1 stores the exact
/// declared scope; it never infers a hierarchy or derives package/module
/// meaning from a path string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ScopeKind {
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

closed_vocabulary!(ScopeKind {
    Global => "GLOBAL",
    Project => "PROJECT",
    Workspace => "WORKSPACE",
    Package => "PACKAGE",
    Module => "MODULE",
    Directory => "DIRECTORY",
    Resource => "RESOURCE",
    Domain => "DOMAIN",
    Task => "TASK",
});

/// Exact declared scope: an axis plus, for every axis narrower than its
/// owning store, a non-empty key. GLOBAL and PROJECT name a whole store
/// and carry no key. A key is never empty, which is what lets the
/// null-safe identity index use `ifnull(scope_key, '')` without a real key
/// colliding with "no key". Fields are private: every value was checked.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KnowledgeScope {
    kind: ScopeKind,
    key: Option<String>,
}

impl KnowledgeScope {
    #[must_use]
    pub const fn global() -> Self {
        Self {
            kind: ScopeKind::Global,
            key: None,
        }
    }

    #[must_use]
    pub const fn project() -> Self {
        Self {
            kind: ScopeKind::Project,
            key: None,
        }
    }

    /// Worktree-specific applicability of a project.db row (#20 D5): the
    /// key is the WorkspaceID, never a path.
    #[must_use]
    pub fn workspace(workspace_id: WorkspaceId) -> Self {
        Self {
            kind: ScopeKind::Workspace,
            key: Some(workspace_id.to_string()),
        }
    }

    /// Any keyed axis; the key is stored verbatim.
    pub fn keyed(kind: ScopeKind, key: impl Into<String>) -> Result<Self, KnowledgeError> {
        Self::checked(kind, Some(key.into()))
    }

    /// Decode stored columns; an unknown kind or bad key is an error.
    pub(crate) fn from_parts(kind: &str, key: Option<String>) -> Result<Self, KnowledgeError> {
        Self::checked(ScopeKind::parse(kind)?, key)
    }

    fn checked(kind: ScopeKind, key: Option<String>) -> Result<Self, KnowledgeError> {
        let keyless = matches!(kind, ScopeKind::Global | ScopeKind::Project);
        let valid = match &key {
            None => keyless,
            Some(key) if keyless || key.is_empty() => false,
            Some(key) => kind != ScopeKind::Workspace || key.parse::<WorkspaceId>().is_ok(),
        };
        if valid {
            Ok(Self { kind, key })
        } else {
            Err(KnowledgeError::InvalidScope {
                kind: kind.as_str(),
                key,
            })
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ScopeKind {
        self.kind
    }

    #[must_use]
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

// ----------------------------------------------------------- provenance

/// Where a durable item came from (#7 task 1 §7, #7 task 4). Whether a
/// given source may *create* a given category is task 4's promotion rule,
/// not a storage check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// The user said it explicitly.
    UserExplicit,
    /// An authoritative project artifact (issue body, design doc).
    AuthoritativeArtifact,
    /// Observed by Brainprint or a tool (Git, filesystem, parser).
    Observed,
    /// Reported by an Agent (statement, inference, proposal).
    AgentReported,
}

closed_vocabulary!(SourceKind {
    UserExplicit => "USER_EXPLICIT",
    AuthoritativeArtifact => "AUTHORITATIVE_ARTIFACT",
    Observed => "OBSERVED",
    AgentReported => "AGENT_REPORTED",
});

/// `source_kind` + `source_locator` (`source_ref` on Work Notes) +
/// `source_revision` (`observed_revision` on state rows). A locator is
/// evidence metadata, never a copy of the source document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub source_kind: SourceKind,
    pub locator: Option<String>,
    pub revision: Option<String>,
}

impl Provenance {
    #[must_use]
    pub const fn new(source_kind: SourceKind) -> Self {
        Self {
            source_kind,
            locator: None,
            revision: None,
        }
    }

    #[must_use]
    pub fn with_locator(mut self, locator: impl Into<String>) -> Self {
        self.locator = Some(locator.into());
        self
    }

    #[must_use]
    pub fn with_revision(mut self, revision: impl Into<String>) -> Self {
        self.revision = Some(revision.into());
        self
    }
}

// ---------------------------------------------------------- typed value

/// Declared type of a [`TypedValue`] (`value_type` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Text,
    Integer,
    Boolean,
    Json,
}

closed_vocabulary!(ValueType {
    Text => "TEXT",
    Integer => "INTEGER",
    Boolean => "BOOLEAN",
    Json => "JSON",
});

/// A Project State / Preference value with an explicit declared type.
/// `Json` is the intentionally generic case and is still a declared type;
/// there is no untyped blob.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedValue {
    Text(String),
    Integer(i64),
    Boolean(bool),
    Json(serde_json::Value),
}

impl TypedValue {
    #[must_use]
    pub const fn value_type(&self) -> ValueType {
        match self {
            Self::Text(_) => ValueType::Text,
            Self::Integer(_) => ValueType::Integer,
            Self::Boolean(_) => ValueType::Boolean,
            Self::Json(_) => ValueType::Json,
        }
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        match self {
            Self::Text(text) => serde_json::Value::from(text.as_str()),
            Self::Integer(number) => serde_json::Value::from(*number),
            Self::Boolean(flag) => serde_json::Value::from(*flag),
            Self::Json(value) => value.clone(),
        }
        .to_string()
    }

    /// Decode a stored pair. JSON that does not match its declared type is
    /// an error, never coerced.
    pub(crate) fn from_parts(value_type: &str, value_json: &str) -> Result<Self, KnowledgeError> {
        let declared = ValueType::parse(value_type)?;
        let invalid = |reason: String| KnowledgeError::InvalidJson {
            what: "typed value",
            reason,
        };
        let json: serde_json::Value =
            serde_json::from_str(value_json).map_err(|error| invalid(error.to_string()))?;
        let mismatch = || invalid(format!("JSON does not hold a {}", declared.as_str()));
        Ok(match declared {
            ValueType::Text => Self::Text(json.as_str().ok_or_else(mismatch)?.to_owned()),
            ValueType::Integer => Self::Integer(json.as_i64().ok_or_else(mismatch)?),
            ValueType::Boolean => Self::Boolean(json.as_bool().ok_or_else(mismatch)?),
            ValueType::Json => Self::Json(json),
        })
    }
}

// --------------------------------------------------------------- policy

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyStatus {
    Active,
    Superseded,
    Disabled,
}

closed_vocabulary!(PolicyStatus {
    Active => "ACTIVE",
    Superseded => "SUPERSEDED",
    Disabled => "DISABLED",
});

/// Policy lineage. P0 requires only SUPERSEDES (#20 D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyLinkKind {
    Supersedes,
}

closed_vocabulary!(PolicyLinkKind {
    Supersedes => "SUPERSEDES",
});

/// Protection class (#13 task 7 §12). Protected classes are what task 2
/// keeps out of reach of project configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectionClass {
    Normal,
    ProtectedPrivacy,
    ProtectedSecurity,
}

closed_vocabulary!(ProtectionClass {
    Normal => "NORMAL",
    ProtectedPrivacy => "PROTECTED_PRIVACY",
    ProtectedSecurity => "PROTECTED_SECURITY",
});

/// Precedence class metadata (#13 task 7 §12: a class, never a numeric
/// ranking score). No design issue defines further classes, so only
/// DEFAULT exists; task 2 extends this if it needs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityClass {
    Default,
}

closed_vocabulary!(PriorityClass {
    Default => "DEFAULT",
});

/// A project Policy (project.db `policy`) or a global user Policy
/// (global.db `user_policy`); the owning store decides which (#20 D1).
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub uid: PolicyId,
    pub scope: KnowledgeScope,
    pub policy_key: Option<String>,
    pub title: String,
    pub rule_text: String,
    pub structured_rule: Option<serde_json::Value>,
    pub protection_class: ProtectionClass,
    pub priority_class: PriorityClass,
    pub status: PolicyStatus,
    pub provenance: Provenance,
    pub created_at: String,
    pub updated_at: String,
}

/// Input for a new Policy. New rows always start ACTIVE; SUPERSEDED is
/// reachable only through an explicit supersession.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPolicy {
    pub scope: KnowledgeScope,
    pub policy_key: Option<String>,
    pub title: String,
    pub rule_text: String,
    pub structured_rule: Option<serde_json::Value>,
    pub protection_class: ProtectionClass,
    pub priority_class: PriorityClass,
    pub provenance: Provenance,
}

/// Direct SUPERSEDES edges of one Policy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PolicyLineage {
    /// Policies this one replaced.
    pub supersedes: Vec<PolicyId>,
    /// Policies that replaced this one.
    pub superseded_by: Vec<PolicyId>,
}

// ------------------------------------------------------------- decision

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionStatus {
    Active,
    Superseded,
    Reversed,
}

closed_vocabulary!(DecisionStatus {
    Active => "ACTIVE",
    Superseded => "SUPERSEDED",
    Reversed => "REVERSED",
});

/// Decision lineage: each kind leaves the older Decision in the matching
/// status. #13's open-ended REFINES has no status counterpart and no
/// caller, so it is not modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionLinkKind {
    Supersedes,
    Reverses,
}

closed_vocabulary!(DecisionLinkKind {
    Supersedes => "SUPERSEDES",
    Reverses => "REVERSES",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub uid: DecisionId,
    pub scope: KnowledgeScope,
    pub topic: String,
    pub chosen_summary: String,
    pub rationale: String,
    pub status: DecisionStatus,
    pub provenance: Provenance,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDecision {
    pub scope: KnowledgeScope,
    pub topic: String,
    pub chosen_summary: String,
    pub rationale: String,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionLink {
    pub kind: DecisionLinkKind,
    pub other: DecisionId,
}

/// Direct lineage edges of one Decision.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DecisionLineage {
    /// `this --kind--> other` (this one superseded/reversed `other`).
    pub outgoing: Vec<DecisionLink>,
    /// `other --kind--> this`.
    pub incoming: Vec<DecisionLink>,
}

// ------------------------------------------------------------ blueprint

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlueprintStatus {
    Draft,
    Active,
    Retired,
}

closed_vocabulary!(BlueprintStatus {
    Draft => "DRAFT",
    Active => "ACTIVE",
    Retired => "RETIRED",
});

/// Which canonical store owns an applied Blueprint definition (#20 D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlueprintOwnerKind {
    /// global.db reusable definition; referenced by value only.
    Global,
    /// project.db project-local definition.
    Project,
}

closed_vocabulary!(BlueprintOwnerKind {
    Global => "GLOBAL",
    Project => "PROJECT",
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlueprintComponent {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlueprintRelationship {
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The structured Blueprint definition (#7 task 1 §3): components,
/// relationships, constraints, stored as `definition_json`. Unknown fields
/// are rejected and relationships must name declared components, so the
/// column cannot become free-form memory.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlueprintDefinition {
    #[serde(default)]
    pub components: Vec<BlueprintComponent>,
    #[serde(default)]
    pub relationships: Vec<BlueprintRelationship>,
    #[serde(default)]
    pub constraints: Vec<String>,
}

impl BlueprintDefinition {
    pub fn validate(&self) -> Result<(), KnowledgeError> {
        let invalid = |reason: &str| {
            Err(KnowledgeError::InvalidJson {
                what: "blueprint definition",
                reason: reason.to_owned(),
            })
        };
        let mut names = BTreeSet::new();
        for component in &self.components {
            if component.name.is_empty() {
                return invalid("component name is empty");
            }
            if !names.insert(component.name.as_str()) {
                return invalid("component names must be unique");
            }
        }
        for relationship in &self.relationships {
            if relationship.kind.is_empty() {
                return invalid("relationship kind is empty");
            }
            if !names.contains(relationship.from.as_str())
                || !names.contains(relationship.to.as_str())
            {
                return invalid("relationship names an undeclared component");
            }
        }
        if self.constraints.iter().any(String::is_empty) {
            return invalid("constraint is empty");
        }
        Ok(())
    }
}

/// A Blueprint definition, global reusable (global.db) or project-local
/// (project.db); same shape in both stores (#20 D2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blueprint {
    pub uid: BlueprintId,
    pub scope: KnowledgeScope,
    pub blueprint_key: Option<String>,
    pub title: String,
    pub intent: String,
    pub definition: BlueprintDefinition,
    pub status: BlueprintStatus,
    pub version: Option<String>,
    pub provenance: Provenance,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBlueprint {
    pub scope: KnowledgeScope,
    pub blueprint_key: Option<String>,
    pub title: String,
    pub intent: String,
    pub definition: BlueprintDefinition,
    /// DRAFT or ACTIVE; a Blueprint is never created RETIRED.
    pub status: BlueprintStatus,
    pub version: Option<String>,
    pub provenance: Provenance,
}

/// Owner-qualified reference to a Blueprint definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlueprintRef {
    pub owner: BlueprintOwnerKind,
    pub uid: BlueprintId,
}

/// Application status. #13 names the column without a vocabulary; P0
/// needs only "applies" and "no longer applies" (terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlueprintApplicationStatus {
    Active,
    Retired,
}

closed_vocabulary!(BlueprintApplicationStatus {
    Active => "ACTIVE",
    Retired => "RETIRED",
});

/// A Blueprint applied inside a Project: a reference and a summary, never
/// a copy of the definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlueprintApplication {
    pub uid: BlueprintApplicationId,
    pub blueprint: BlueprintRef,
    pub scope: KnowledgeScope,
    pub status: BlueprintApplicationStatus,
    pub application_summary: String,
    pub provenance: Provenance,
    pub created_at: String,
    pub updated_at: String,
}

/// New applications start ACTIVE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBlueprintApplication {
    pub blueprint: BlueprintRef,
    pub scope: KnowledgeScope,
    pub application_summary: String,
    pub provenance: Provenance,
}

// -------------------------------------------------------- project state

/// Project State status. #13 names the column without a vocabulary; a
/// fact is either current or retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStateStatus {
    Current,
    Retired,
}

closed_vocabulary!(ProjectStateStatus {
    Current => "CURRENT",
    Retired => "RETIRED",
});

/// A current project fact -- a fact, not a rule (#7 task 2 §5). Identity
/// is `(key, scope)`; `uid` is the stable handle a promoted Work Note
/// references across DBs.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectState {
    pub uid: ProjectStateId,
    pub key: String,
    pub scope: KnowledgeScope,
    pub value: TypedValue,
    pub status: ProjectStateStatus,
    /// `revision` is stored as `observed_revision`.
    pub provenance: Provenance,
    pub updated_at: String,
}

/// workspace.db `workspace_project_state`: the same semantics as
/// [`ProjectState`], owned by one Workspace (#20 D5).
pub type WorkspaceProjectState = ProjectState;

/// Set the current value of `(key, scope)`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectStateUpdate {
    pub key: String,
    pub scope: KnowledgeScope,
    pub value: TypedValue,
    pub status: ProjectStateStatus,
    pub provenance: Provenance,
}

// ----------------------------------------------------------- preference

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferenceStatus {
    Active,
    Superseded,
    Disabled,
}

closed_vocabulary!(PreferenceStatus {
    Active => "ACTIVE",
    Superseded => "SUPERSEDED",
    Disabled => "DISABLED",
});

/// A global user Preference. Not a Policy (#20 D1).
#[derive(Debug, Clone, PartialEq)]
pub struct UserPreference {
    pub uid: UserPreferenceId,
    pub scope: KnowledgeScope,
    pub preference_key: String,
    pub value: TypedValue,
    pub status: PreferenceStatus,
    pub provenance: Provenance,
    pub created_at: String,
    pub updated_at: String,
}

/// New preferences start ACTIVE.
#[derive(Debug, Clone, PartialEq)]
pub struct NewUserPreference {
    pub scope: KnowledgeScope,
    pub preference_key: String,
    pub value: TypedValue,
    pub provenance: Provenance,
}

// ------------------------------------------------------------ work item

/// External correlation kind for `work_item.source_kind` (#20 D6: the
/// existing `source_kind + source_ref` is the correlation baseline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkItemSourceKind {
    Issue,
    ExternalTask,
    UserRequest,
}

closed_vocabulary!(WorkItemSourceKind {
    Issue => "ISSUE",
    ExternalTask => "EXTERNAL_TASK",
    UserRequest => "USER_REQUEST",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkItemStatus {
    Open,
    Active,
    Blocked,
    Paused,
    Completed,
    Abandoned,
}

closed_vocabulary!(WorkItemStatus {
    Open => "OPEN",
    Active => "ACTIVE",
    Blocked => "BLOCKED",
    Paused => "PAUSED",
    Completed => "COMPLETED",
    Abandoned => "ABANDONED",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub uid: WorkItemId,
    pub source_kind: WorkItemSourceKind,
    pub source_ref: Option<String>,
    pub title: Option<String>,
    pub goal: String,
    pub status: WorkItemStatus,
    pub created_at: String,
    pub closed_at: Option<String>,
}

/// New WorkItems start OPEN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorkItem {
    pub source_kind: WorkItemSourceKind,
    pub source_ref: Option<String>,
    pub title: Option<String>,
    pub goal: String,
}

/// Stored dirty observation state (`baseline_dirty_state` /
/// `remaining_dirty_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyState {
    Unknown,
    Clean,
    Dirty,
}

closed_vocabulary!(DirtyState {
    Unknown => "UNKNOWN",
    Clean => "CLEAN",
    Dirty => "DIRTY",
});

/// Whether uncommitted changes were observed (#20 task 3 LOCKED §7-8).
/// UNKNOWN means nobody looked; it is never read as CLEAN, and a missing
/// fingerprint alone never means clean. Only DIRTY carries a fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirtyObservation {
    Unknown,
    Clean,
    Dirty { fingerprint: String },
}

impl DirtyObservation {
    /// Decode (or check) a state/fingerprint pair. UNKNOWN or CLEAN with a
    /// fingerprint, and DIRTY without a non-empty one, are rejected.
    pub fn from_parts(
        state: DirtyState,
        fingerprint: Option<String>,
    ) -> Result<Self, KnowledgeError> {
        match (state, fingerprint) {
            (DirtyState::Unknown, None) => Ok(Self::Unknown),
            (DirtyState::Clean, None) => Ok(Self::Clean),
            (DirtyState::Dirty, Some(fingerprint)) if !fingerprint.is_empty() => {
                Ok(Self::Dirty { fingerprint })
            }
            (state, fingerprint) => Err(KnowledgeError::Inconsistent {
                table: "dirty observation",
                reason: format!("{} with fingerprint {fingerprint:?}", state.as_str()),
            }),
        }
    }

    #[must_use]
    pub const fn state(&self) -> DirtyState {
        match self {
            Self::Unknown => DirtyState::Unknown,
            Self::Clean => DirtyState::Clean,
            Self::Dirty { .. } => DirtyState::Dirty,
        }
    }

    #[must_use]
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Dirty { fingerprint } => Some(fingerprint),
            Self::Unknown | Self::Clean => None,
        }
    }

    /// Reject a DIRTY observation built with an empty fingerprint.
    pub fn validate(&self) -> Result<(), KnowledgeError> {
        Self::from_parts(self.state(), self.fingerprint().map(str::to_owned)).map(drop)
    }
}

/// Current operational snapshot of one WorkItem -- a snapshot, never an
/// event log (#13 task 7 §17). The `baseline_*` fields are fixed by the
/// first activation and never rewritten by the lifecycle (#20 task 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingState {
    pub work_item: WorkItemId,
    pub baseline_workspace_revision: String,
    /// Value reference into index.db; no FK (#13 task 7 §21).
    pub baseline_generation_no: i64,
    pub baseline_head: Option<String>,
    pub baseline_dirty: DirtyObservation,
    pub current_step: Option<String>,
    pub progress_summary: Option<String>,
    pub remaining_summary: Option<String>,
    pub blocker_summary: Option<String>,
    /// Observed attribution hint, not a session identity (#20 D6).
    pub owner_agent: Option<String>,
    pub last_observed_workspace_revision: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkResourceRole {
    Target,
    Touched,
    Owned,
    Related,
    PreexistingDirty,
}

closed_vocabulary!(WorkResourceRole {
    Target => "TARGET",
    Touched => "TOUCHED",
    Owned => "OWNED",
    Related => "RELATED",
    PreexistingDirty => "PREEXISTING_DIRTY",
});

/// A WorkItem ↔ Resource link. `resource` is a stable-ID value: the
/// Resource lives in index.db and may be rebuilt independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkResource {
    pub work_item: WorkItemId,
    pub resource: ResourceId,
    pub role: WorkResourceRole,
    pub locator_hint: Option<String>,
    pub first_observed_revision: String,
    pub last_observed_revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkResultStatus {
    Completed,
    Partial,
    Abandoned,
}

closed_vocabulary!(WorkResultStatus {
    Completed => "COMPLETED",
    Partial => "PARTIAL",
    Abandoned => "ABANDONED",
});

/// A recorded result. A `commit_id` never implies COMPLETED (#13 task 7
/// §19). On write, `created_at` is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkResult {
    pub work_item: WorkItemId,
    pub result_status: WorkResultStatus,
    pub result_summary: String,
    pub commit_id: Option<String>,
    pub change_set_fingerprint: Option<String>,
    pub verification_summary: Option<String>,
    pub result_workspace_revision: String,
    pub result_generation_no: Option<i64>,
    /// Dirty state observed when the result was recorded.
    pub remaining_dirty: DirtyObservation,
    pub created_at: String,
}

/// A compact handoff snapshot. On write, `created_at` is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkHandoff {
    pub work_item: WorkItemId,
    pub handoff_summary: String,
    pub remaining_summary: Option<String>,
    pub blocker_summary: Option<String>,
    pub next_scope_hint: Option<String>,
    pub created_at: String,
}

// ------------------------------------------------------------ work note

/// Task-local, not-yet-canonical knowledge (#20 D4). A PROPOSAL stays a
/// Work Note; it is never a Policy/Decision row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkNoteKind {
    Observation,
    Proposal,
    OpenQuestion,
}

closed_vocabulary!(WorkNoteKind {
    Observation => "OBSERVATION",
    Proposal => "PROPOSAL",
    OpenQuestion => "OPEN_QUESTION",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkNoteStatus {
    Open,
    Resolved,
    Promoted,
    Discarded,
}

closed_vocabulary!(WorkNoteStatus {
    Open => "OPEN",
    Resolved => "RESOLVED",
    Promoted => "PROMOTED",
    Discarded => "DISCARDED",
});

/// The canonical project.db row a PROMOTED note points at, by stable-ID
/// value (no cross-DB FK). Only task 4's promotion writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotedItem {
    Policy(PolicyId),
    Decision(DecisionId),
    ProjectState(ProjectStateId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromotedItemKind {
    Policy,
    Decision,
    ProjectState,
}

closed_vocabulary!(PromotedItemKind {
    Policy => "POLICY",
    Decision => "DECISION",
    ProjectState => "PROJECT_STATE",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkNote {
    pub uid: WorkNoteId,
    pub work_item: WorkItemId,
    pub kind: WorkNoteKind,
    pub note_text: String,
    pub status: WorkNoteStatus,
    /// `locator` is stored as `source_ref`.
    pub provenance: Provenance,
    /// `Some` exactly when `status` is PROMOTED.
    pub promoted_item: Option<PromotedItem>,
    pub created_at: String,
    pub updated_at: String,
}

/// New notes start OPEN with no promoted target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorkNote {
    pub kind: WorkNoteKind,
    pub note_text: String,
    pub provenance: Provenance,
}
