//! WorkNote → project-shared truth promotion (#20 task 4).
//!
//! [`PromotionRuntime`] is the one high-level path from a Workspace's
//! Observation / Proposal ([`WorkNote`]) to a project.db Policy, Decision
//! or Project State. It is bound to one Project, one of its Workspaces,
//! the Project's canonical project-home project.db and that Workspace's
//! workspace.db.
//!
//! - Authority is a separate, typed [`PromotionEvidence`]; the note's own
//!   provenance (often AGENT_REPORTED) is kept on the note and the
//!   receipt but never promotes anything by itself.
//! - The target payload is given explicitly; `note_text` is never parsed.
//! - Lineage (supersede / reverse) is only what the request names.
//! - project.db and workspace.db are separate WAL databases, so there is no
//!   cross-DB transaction. The project phase (target + lineage + receipt)
//!   commits atomically in project.db; the note is marked PROMOTED after.
//!   A retry finds the `knowledge_promotion` receipt and only finishes the
//!   note -- it never re-applies the target.

use std::{error::Error, fmt};

use brainprint_core::{
    DecisionId, PolicyId, ProjectId, ProjectStateId, PromotionId, WorkNoteId, WorkspaceId,
};
use rusqlite::{Row, params};

use super::{
    DECISION, DecisionLinkKind, DecisionStatus, KnowledgeError, KnowledgeScope, NewDecision,
    NewPolicy, PROJECT_POLICY, PolicyLinkKind, PolicyStatus, PriorityClass, ProjectKnowledgeStore,
    ProjectStateStatus, ProjectStateUpdate, PromotedItem, PromotedItemKind, ProtectionClass,
    Provenance, ScopeKind, SourceKind, Store, TypedValue, WorkNote, WorkNoteKind, WorkNoteStatus,
    WorkspaceKnowledgeStore, blob, link_and_retire_in, query_one, require_owner, uid_column,
    uid_from_blob,
};
use crate::{
    db,
    paths::WorkspacePaths,
    registry::GlobalRegistry,
    registry::RegistryError,
    resolution::{UnknownAxisValue, closed_vocabulary},
};

// --------------------------------------------------------------- evidence

/// Why a promotion is authoritative (#20 task 4 LOCKED §1). Not a rank:
/// each basis names exactly one acceptable [`SourceKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionBasis {
    UserExplicit,
    AuthoritativeArtifact,
    /// Evidence a trusted Core workflow already validated at project
    /// level. Project State only; task 4 runs no command to produce it.
    ValidatedProjectObservation,
}

closed_vocabulary!(PromotionBasis {
    UserExplicit => "USER_EXPLICIT",
    AuthoritativeArtifact => "AUTHORITATIVE_ARTIFACT",
    ValidatedProjectObservation => "VALIDATED_PROJECT_OBSERVATION",
});

/// The one canonical authority of a promotion. Its provenance becomes the
/// target's provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionEvidence {
    pub basis: PromotionBasis,
    pub provenance: Provenance,
}

impl PromotionEvidence {
    /// AGENT_REPORTED never validates; nothing is coerced.
    pub fn validate(&self) -> Result<(), PromotionError> {
        let present = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.is_empty());
        let (required, needs_locator, needs_revision) = match self.basis {
            PromotionBasis::UserExplicit => (SourceKind::UserExplicit, false, false),
            PromotionBasis::AuthoritativeArtifact => {
                (SourceKind::AuthoritativeArtifact, true, false)
            }
            PromotionBasis::ValidatedProjectObservation => (SourceKind::Observed, true, true),
        };
        let invalid = |reason: String| Err(PromotionError::InvalidEvidence(reason));
        if self.provenance.source_kind != required {
            return invalid(format!(
                "{} needs {} provenance, got {}",
                self.basis, required, self.provenance.source_kind
            ));
        }
        if needs_locator && !present(&self.provenance.locator) {
            return invalid(format!("{} needs a locator", self.basis));
        }
        if needs_revision && !present(&self.provenance.revision) {
            return invalid(format!("{} needs a revision", self.basis));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------- target

/// Explicit Policy payload. There is no provenance field: the target's
/// provenance is the promotion evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyTarget {
    pub scope: KnowledgeScope,
    pub policy_key: Option<String>,
    pub title: String,
    pub rule_text: String,
    pub structured_rule: Option<serde_json::Value>,
    pub protection_class: ProtectionClass,
    pub priority_class: PriorityClass,
    /// ACTIVE Policy with exactly this scope and key to supersede.
    pub supersedes: Option<PolicyId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionLineageAction {
    Supersedes(DecisionId),
    Reverses(DecisionId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionTarget {
    pub scope: KnowledgeScope,
    pub topic: String,
    pub chosen_summary: String,
    pub rationale: String,
    /// ACTIVE Decision with exactly this scope and topic to replace.
    pub lineage: Option<DecisionLineageAction>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectStateTarget {
    /// A project-shared scope; never WORKSPACE (workspace-local facts
    /// already live in `workspace_project_state`).
    pub scope: KnowledgeScope,
    pub key: String,
    pub value: TypedValue,
    pub status: ProjectStateStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromotionTarget {
    Policy(PolicyTarget),
    Decision(DecisionTarget),
    ProjectState(ProjectStateTarget),
}

impl PromotionTarget {
    const fn kind(&self) -> PromotedItemKind {
        match self {
            Self::Policy(_) => PromotedItemKind::Policy,
            Self::Decision(_) => PromotedItemKind::Decision,
            Self::ProjectState(_) => PromotedItemKind::ProjectState,
        }
    }

    const fn lineage(&self) -> Option<PromotionLineage> {
        match self {
            Self::Policy(policy) => match policy.supersedes {
                Some(old) => Some(PromotionLineage::PolicySupersedes(old)),
                None => None,
            },
            Self::Decision(decision) => match decision.lineage {
                Some(DecisionLineageAction::Supersedes(old)) => {
                    Some(PromotionLineage::DecisionSupersedes(old))
                }
                Some(DecisionLineageAction::Reverses(old)) => {
                    Some(PromotionLineage::DecisionReverses(old))
                }
                None => None,
            },
            Self::ProjectState(_) => None,
        }
    }
}

/// Promote one WorkNote of the bound Workspace, named explicitly.
#[derive(Debug, Clone, PartialEq)]
pub struct PromotionRequest {
    pub work_note: WorkNoteId,
    pub evidence: PromotionEvidence,
    pub target: PromotionTarget,
}

// ---------------------------------------------------------------- receipt

/// The lineage action a promotion applied, with the retired row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionLineage {
    PolicySupersedes(PolicyId),
    DecisionSupersedes(DecisionId),
    DecisionReverses(DecisionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromotionLineageKind {
    PolicySupersedes,
    DecisionSupersedes,
    DecisionReverses,
}

closed_vocabulary!(PromotionLineageKind {
    PolicySupersedes => "POLICY_SUPERSEDES",
    DecisionSupersedes => "DECISION_SUPERSEDES",
    DecisionReverses => "DECISION_REVERSES",
});

impl PromotionLineage {
    fn parts(self) -> (PromotionLineageKind, [u8; 16]) {
        match self {
            Self::PolicySupersedes(id) => (PromotionLineageKind::PolicySupersedes, id.to_bytes()),
            Self::DecisionSupersedes(id) => {
                (PromotionLineageKind::DecisionSupersedes, id.to_bytes())
            }
            Self::DecisionReverses(id) => (PromotionLineageKind::DecisionReverses, id.to_bytes()),
        }
    }
}

/// project.db `knowledge_promotion`: one row per promoted WorkNote. It
/// keeps both sides of provenance -- the note that led to the target
/// (kind, source kind, fingerprint) and the evidence that authorized it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionReceipt {
    pub uid: PromotionId,
    pub workspace: WorkspaceId,
    pub work_note: WorkNoteId,
    pub work_note_kind: WorkNoteKind,
    pub work_note_source_kind: SourceKind,
    pub work_note_fingerprint: String,
    pub target: PromotedItem,
    pub request_fingerprint: String,
    pub evidence: PromotionEvidence,
    pub lineage: Option<PromotionLineage>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// Target and receipt written now; note marked.
    Created,
    /// The receipt already existed (the previous attempt stopped between
    /// the two DBs); only the note was marked now.
    Reconciled,
    /// Receipt and note were both already done; nothing written.
    AlreadyPromoted,
}

/// Engine-internal promotion result; not a projection or transport shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionResult {
    pub receipt: PromotionReceipt,
    pub outcome: PromotionOutcome,
}

// ------------------------------------------------------------------ error

#[derive(Debug)]
pub enum PromotionError {
    Knowledge(KnowledgeError),
    UnknownWorkspace(WorkspaceId),
    WorkspaceNotInProject {
        workspace: WorkspaceId,
        expected: ProjectId,
        found: ProjectId,
    },
    /// The Workspace's workspace.db does not exist; it is never created.
    MissingWorkspaceDb,
    /// workspace.db is unbound or bound to another Workspace.
    WorkspaceDbMismatch {
        expected: WorkspaceId,
        found: Option<WorkspaceId>,
    },
    InvalidEvidence(String),
    /// Not one of PROPOSAL → POLICY / DECISION, OBSERVATION → PROJECT_STATE.
    CategoryNotAllowed {
        note_kind: WorkNoteKind,
        target: &'static str,
    },
    /// The evidence basis may not create this category (or this note).
    AuthorityNotAllowed {
        basis: PromotionBasis,
        target: &'static str,
        reason: &'static str,
    },
    /// Scope or lineage of the target is not acceptable.
    InvalidTarget(String),
    NoteNotPromotable(WorkNoteStatus),
    /// A receipt exists for this note but records a different request.
    IdempotencyConflict(PromotionId),
    /// Receipt, note and target disagree; nothing is chosen or repaired.
    CorruptPromotionState(String),
}

impl fmt::Display for PromotionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Knowledge(source) => write!(formatter, "{source}"),
            Self::UnknownWorkspace(id) => write!(formatter, "Workspace {id} is not registered"),
            Self::WorkspaceNotInProject {
                workspace,
                expected,
                found,
            } => write!(
                formatter,
                "Workspace {workspace} belongs to Project {found}, not {expected}"
            ),
            Self::MissingWorkspaceDb => formatter.write_str("workspace.db does not exist"),
            Self::WorkspaceDbMismatch { expected, found } => write!(
                formatter,
                "workspace.db is bound to {found:?}, expected {expected}"
            ),
            Self::InvalidEvidence(reason) => write!(formatter, "invalid evidence: {reason}"),
            Self::CategoryNotAllowed { note_kind, target } => {
                write!(formatter, "a {note_kind} note cannot become a {target}")
            }
            Self::AuthorityNotAllowed {
                basis,
                target,
                reason,
            } => write!(formatter, "{basis} cannot promote a {target}: {reason}"),
            Self::InvalidTarget(reason) => write!(formatter, "invalid target: {reason}"),
            Self::NoteNotPromotable(status) => {
                write!(formatter, "a {status} note is not promotable")
            }
            Self::IdempotencyConflict(id) => write!(
                formatter,
                "promotion {id} already recorded a different request for this note"
            ),
            Self::CorruptPromotionState(reason) => {
                write!(formatter, "corrupt promotion state: {reason}")
            }
        }
    }
}

impl Error for PromotionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Knowledge(source) => Some(source),
            _ => None,
        }
    }
}

impl From<KnowledgeError> for PromotionError {
    fn from(source: KnowledgeError) -> Self {
        Self::Knowledge(source)
    }
}

impl From<RegistryError> for PromotionError {
    fn from(source: RegistryError) -> Self {
        Self::Knowledge(source.into())
    }
}

impl From<rusqlite::Error> for PromotionError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Knowledge(source.into())
    }
}

// ---------------------------------------------------------------- runtime

/// A request that passed every gate, with its note and fingerprints.
struct Checked<'a> {
    request: &'a PromotionRequest,
    note: WorkNote,
    note_fingerprint: String,
    request_fingerprint: String,
}

/// What the project phase found or did.
enum ProjectPhase {
    Committed(PromotionReceipt),
    /// A receipt appeared first (another attempt won); reconcile with it.
    AlreadyReceipted(PromotionReceipt),
}

pub struct PromotionRuntime {
    project_id: ProjectId,
    workspace_id: WorkspaceId,
    project: ProjectKnowledgeStore,
    workspace: WorkspaceKnowledgeStore,
}

impl PromotionRuntime {
    /// Bind to `workspace_id` of `project_id`: the registry must place the
    /// Workspace in the Project, project.db is the Project's registered
    /// project-home file (bound to it), and workspace.db is the
    /// Workspace's own (bound to it). Nothing is created or bound here.
    pub fn open(
        registry: &GlobalRegistry,
        project_id: ProjectId,
        workspace_id: WorkspaceId,
    ) -> Result<Self, PromotionError> {
        let entry = registry
            .get_workspace(workspace_id)?
            .ok_or(PromotionError::UnknownWorkspace(workspace_id))?;
        if entry.project_id != project_id {
            return Err(PromotionError::WorkspaceNotInProject {
                workspace: workspace_id,
                expected: project_id,
                found: entry.project_id,
            });
        }
        let project = ProjectKnowledgeStore::open_project_home(registry, project_id)?;
        let workspace_db = WorkspacePaths::from_root(&entry.locator).workspace_db;
        if !workspace_db.is_file() {
            return Err(PromotionError::MissingWorkspaceDb);
        }
        let workspace = WorkspaceKnowledgeStore::open(&workspace_db)?;
        let bound = workspace.bound_workspace_id()?;
        if bound != Some(workspace_id) {
            return Err(PromotionError::WorkspaceDbMismatch {
                expected: workspace_id,
                found: bound,
            });
        }
        Ok(Self {
            project_id,
            workspace_id,
            project,
            workspace,
        })
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// Promote `request.work_note`, or finish / confirm an earlier
    /// promotion of it. See the module docs for the protocol.
    pub fn promote(&self, request: &PromotionRequest) -> Result<PromotionResult, PromotionError> {
        let checked = self.check(request)?;
        if let Some(receipt) = self.receipt(request.work_note)? {
            return self.reconcile(&checked, receipt);
        }
        match checked.note.status {
            WorkNoteStatus::Open => {}
            WorkNoteStatus::Promoted => {
                return Err(PromotionError::CorruptPromotionState(
                    "note is PROMOTED but project.db has no receipt for it".to_owned(),
                ));
            }
            status => return Err(PromotionError::NoteNotPromotable(status)),
        }
        match self.project_phase(&checked)? {
            ProjectPhase::Committed(receipt) => self.mark(receipt, PromotionOutcome::Created),
            ProjectPhase::AlreadyReceipted(receipt) => self.reconcile(&checked, receipt),
        }
    }

    /// The receipt of `work_note`'s promotion in this Workspace, if any.
    pub fn receipt(
        &self,
        work_note: WorkNoteId,
    ) -> Result<Option<PromotionReceipt>, PromotionError> {
        Ok(query_one(
            self.project.connection(),
            &format!(
                "SELECT {RECEIPT_COLUMNS} FROM knowledge_promotion \
                 WHERE workspace_uid = ?1 AND work_note_uid = ?2"
            ),
            params![blob(self.workspace_id), blob(work_note)],
            decode_receipt,
        )?)
    }

    // ---- gates ----

    fn check<'a>(&self, request: &'a PromotionRequest) -> Result<Checked<'a>, PromotionError> {
        request.evidence.validate()?;
        let note = self
            .workspace
            .get_work_note(request.work_note)?
            .ok_or_else(|| KnowledgeError::NotFound {
                what: "work_note",
                uid: request.work_note.to_string(),
            })?;
        let target = request.target.kind();
        let basis = request.evidence.basis;
        let category_ok = matches!(
            (note.kind, target),
            (
                WorkNoteKind::Proposal,
                PromotedItemKind::Policy | PromotedItemKind::Decision
            ) | (WorkNoteKind::Observation, PromotedItemKind::ProjectState)
        );
        if !category_ok {
            return Err(PromotionError::CategoryNotAllowed {
                note_kind: note.kind,
                target: target.as_str(),
            });
        }
        let refuse = |reason| PromotionError::AuthorityNotAllowed {
            basis,
            target: target.as_str(),
            reason,
        };
        if basis == PromotionBasis::ValidatedProjectObservation {
            if target != PromotedItemKind::ProjectState {
                return Err(refuse("a validated observation is not an adoption"));
            }
            if note.provenance.source_kind != SourceKind::Observed {
                return Err(refuse("the source note itself must be OBSERVED"));
            }
        }
        self.check_scope(&request.target)?;
        Ok(Checked {
            note_fingerprint: note_fingerprint(&note),
            request_fingerprint: request_fingerprint(request),
            request,
            note,
        })
    }

    fn check_scope(&self, target: &PromotionTarget) -> Result<(), PromotionError> {
        let scope = match target {
            PromotionTarget::Policy(policy) => &policy.scope,
            PromotionTarget::Decision(decision) => &decision.scope,
            PromotionTarget::ProjectState(state) => {
                if state.scope.kind() == ScopeKind::Workspace {
                    return Err(PromotionError::InvalidTarget(
                        "a WORKSPACE fact is workspace_project_state, not project truth".to_owned(),
                    ));
                }
                &state.scope
            }
        };
        require_owner(scope, Store::Project).map_err(|_| {
            PromotionError::InvalidTarget(format!("{scope:?} is not project-owned"))
        })?;
        let own_key = self.workspace_id.to_string();
        if scope.kind() == ScopeKind::Workspace && scope.key() != Some(own_key.as_str()) {
            return Err(PromotionError::InvalidTarget(format!(
                "a note of Workspace {own_key} cannot write {scope:?}"
            )));
        }
        Ok(())
    }

    // ---- retry ----

    fn reconcile(
        &self,
        checked: &Checked<'_>,
        receipt: PromotionReceipt,
    ) -> Result<PromotionResult, PromotionError> {
        if receipt.work_note_fingerprint != checked.note_fingerprint {
            return Err(PromotionError::CorruptPromotionState(format!(
                "receipt {} was written for different note content",
                receipt.uid
            )));
        }
        if receipt.request_fingerprint != checked.request_fingerprint {
            return Err(PromotionError::IdempotencyConflict(receipt.uid));
        }
        // Existence and identity only: a later supersession or state
        // update does not make the historical target wrong.
        if !self.target_exists(receipt.target)? {
            return Err(PromotionError::CorruptPromotionState(format!(
                "receipt {} names {:?}, which project.db no longer has",
                receipt.uid, receipt.target
            )));
        }
        match checked.note.status {
            WorkNoteStatus::Open => self.mark(receipt, PromotionOutcome::Reconciled),
            WorkNoteStatus::Promoted if checked.note.promoted_item == Some(receipt.target) => {
                Ok(PromotionResult {
                    receipt,
                    outcome: PromotionOutcome::AlreadyPromoted,
                })
            }
            status => Err(PromotionError::CorruptPromotionState(format!(
                "note is {status} ({:?}) but receipt {} names {:?}",
                checked.note.promoted_item, receipt.uid, receipt.target
            ))),
        }
    }

    fn target_exists(&self, target: PromotedItem) -> Result<bool, PromotionError> {
        Ok(match target {
            PromotedItem::Policy(id) => self.project.get_policy(id)?.is_some(),
            PromotedItem::Decision(id) => self.project.get_decision(id)?.is_some(),
            PromotedItem::ProjectState(id) => self.project.get_project_state_by_uid(id)?.is_some(),
        })
    }

    /// Workspace phase: runs only after the project phase committed.
    fn mark(
        &self,
        receipt: PromotionReceipt,
        outcome: PromotionOutcome,
    ) -> Result<PromotionResult, PromotionError> {
        self.workspace
            .mark_work_note_promoted(receipt.work_note, receipt.target)?;
        Ok(PromotionResult { receipt, outcome })
    }

    // ---- project phase ----

    /// One project.db transaction: lineage checks, target, lineage writes,
    /// receipt. Any failure rolls every one of them back.
    fn project_phase(&self, checked: &Checked<'_>) -> Result<ProjectPhase, PromotionError> {
        let transaction = self.project.begin()?;
        if let Some(existing) = self.receipt(checked.request.work_note)? {
            return Ok(ProjectPhase::AlreadyReceipted(existing));
        }
        let provenance = checked.request.evidence.provenance.clone();
        let target = match &checked.request.target {
            PromotionTarget::Policy(policy) => {
                PromotedItem::Policy(self.promote_policy(policy, provenance)?)
            }
            PromotionTarget::Decision(decision) => {
                PromotedItem::Decision(self.promote_decision(decision, provenance)?)
            }
            PromotionTarget::ProjectState(state) => PromotedItem::ProjectState(
                self.project
                    .upsert_project_state(&ProjectStateUpdate {
                        key: state.key.clone(),
                        scope: state.scope.clone(),
                        value: state.value.clone(),
                        status: state.status,
                        provenance,
                    })?
                    .uid,
            ),
        };
        let receipt = PromotionReceipt {
            uid: PromotionId::generate(),
            workspace: self.workspace_id,
            work_note: checked.note.uid,
            work_note_kind: checked.note.kind,
            work_note_source_kind: checked.note.provenance.source_kind,
            work_note_fingerprint: checked.note_fingerprint.clone(),
            target,
            request_fingerprint: checked.request_fingerprint.clone(),
            evidence: checked.request.evidence.clone(),
            lineage: checked.request.target.lineage(),
            created_at: db::now_millis_text(),
        };
        self.insert_receipt(&receipt)?;
        transaction.commit()?;
        Ok(ProjectPhase::Committed(receipt))
    }

    fn promote_policy(
        &self,
        policy: &PolicyTarget,
        provenance: Provenance,
    ) -> Result<PolicyId, PromotionError> {
        if let Some(old) = policy.supersedes {
            let current = self.project.get_policy(old)?;
            let valid = current.as_ref().is_some_and(|current| {
                current.status == PolicyStatus::Active
                    && current.scope == policy.scope
                    && current.policy_key == policy.policy_key
            });
            if !valid {
                return Err(PromotionError::InvalidTarget(format!(
                    "Policy {old} is not an ACTIVE Policy with the same scope and key: {current:?}"
                )));
            }
        }
        let created = self.project.insert_policy(&NewPolicy {
            scope: policy.scope.clone(),
            policy_key: policy.policy_key.clone(),
            title: policy.title.clone(),
            rule_text: policy.rule_text.clone(),
            structured_rule: policy.structured_rule.clone(),
            protection_class: policy.protection_class,
            priority_class: policy.priority_class,
            provenance,
        })?;
        if let Some(old) = policy.supersedes {
            link_and_retire_in(
                self.project.connection(),
                &PROJECT_POLICY,
                created.uid,
                old,
                PolicyLinkKind::Supersedes.as_str(),
                Some(PolicyStatus::Superseded.as_str()),
            )?;
        }
        Ok(created.uid)
    }

    fn promote_decision(
        &self,
        decision: &DecisionTarget,
        provenance: Provenance,
    ) -> Result<DecisionId, PromotionError> {
        let lineage = decision.lineage.map(|action| match action {
            DecisionLineageAction::Supersedes(old) => (
                old,
                DecisionLinkKind::Supersedes,
                DecisionStatus::Superseded,
            ),
            DecisionLineageAction::Reverses(old) => {
                (old, DecisionLinkKind::Reverses, DecisionStatus::Reversed)
            }
        });
        if let Some((old, _, _)) = lineage {
            let current = self.project.get_decision(old)?;
            let valid = current.as_ref().is_some_and(|current| {
                current.status == DecisionStatus::Active
                    && current.scope == decision.scope
                    && current.topic == decision.topic
            });
            if !valid {
                return Err(PromotionError::InvalidTarget(format!(
                    "Decision {old} is not an ACTIVE Decision with the same scope and topic: \
                     {current:?}"
                )));
            }
        }
        let created = self.project.insert_decision(&NewDecision {
            scope: decision.scope.clone(),
            topic: decision.topic.clone(),
            chosen_summary: decision.chosen_summary.clone(),
            rationale: decision.rationale.clone(),
            provenance,
        })?;
        if let Some((old, link, retired)) = lineage {
            link_and_retire_in(
                self.project.connection(),
                &DECISION,
                created.uid,
                old,
                link.as_str(),
                Some(retired.as_str()),
            )?;
        }
        Ok(created.uid)
    }

    fn insert_receipt(&self, receipt: &PromotionReceipt) -> Result<(), PromotionError> {
        let (target_kind, target_uid) = target_parts(receipt.target);
        let lineage = receipt.lineage.map(PromotionLineage::parts);
        self.project.connection().execute(
            &format!(
                "INSERT INTO knowledge_promotion ({RECEIPT_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
            ),
            params![
                blob(receipt.uid),
                blob(receipt.workspace),
                blob(receipt.work_note),
                receipt.work_note_kind.as_str(),
                receipt.work_note_source_kind.as_str(),
                receipt.work_note_fingerprint,
                target_kind.as_str(),
                target_uid.to_vec(),
                receipt.request_fingerprint,
                receipt.evidence.basis.as_str(),
                receipt.evidence.provenance.source_kind.as_str(),
                receipt.evidence.provenance.locator,
                receipt.evidence.provenance.revision,
                lineage.map(|(kind, _)| kind.as_str()),
                lineage.map(|(_, uid)| uid.to_vec()),
                receipt.created_at,
            ],
        )?;
        Ok(())
    }
}

const RECEIPT_COLUMNS: &str = "uid, workspace_uid, work_note_uid, work_note_kind, \
    work_note_source_kind, work_note_fingerprint, target_kind, target_uid, request_fingerprint, \
    promotion_basis, authority_source_kind, authority_source_locator, authority_source_revision, \
    lineage_kind, lineage_target_uid, created_at";

fn target_parts(target: PromotedItem) -> (PromotedItemKind, [u8; 16]) {
    match target {
        PromotedItem::Policy(id) => (PromotedItemKind::Policy, id.to_bytes()),
        PromotedItem::Decision(id) => (PromotedItemKind::Decision, id.to_bytes()),
        PromotedItem::ProjectState(id) => (PromotedItemKind::ProjectState, id.to_bytes()),
    }
}

fn decode_receipt(row: &Row<'_>) -> Result<PromotionReceipt, KnowledgeError> {
    const TABLE: &str = "knowledge_promotion";
    let target_uid: Vec<u8> = row.get(7)?;
    let target = match PromotedItemKind::parse(&row.get::<_, String>(6)?)? {
        PromotedItemKind::Policy => PromotedItem::Policy(uid_from_blob(&target_uid, TABLE)?),
        PromotedItemKind::Decision => PromotedItem::Decision(uid_from_blob(&target_uid, TABLE)?),
        PromotedItemKind::ProjectState => {
            PromotedItem::ProjectState(uid_from_blob::<ProjectStateId>(&target_uid, TABLE)?)
        }
    };
    let lineage_kind: Option<String> = row.get(13)?;
    let lineage_uid: Option<Vec<u8>> = row.get(14)?;
    let lineage = match (lineage_kind, lineage_uid) {
        (None, None) => None,
        (Some(kind), Some(uid)) => Some(match PromotionLineageKind::parse(&kind)? {
            PromotionLineageKind::PolicySupersedes => {
                PromotionLineage::PolicySupersedes(uid_from_blob(&uid, TABLE)?)
            }
            PromotionLineageKind::DecisionSupersedes => {
                PromotionLineage::DecisionSupersedes(uid_from_blob(&uid, TABLE)?)
            }
            PromotionLineageKind::DecisionReverses => {
                PromotionLineage::DecisionReverses(uid_from_blob(&uid, TABLE)?)
            }
        }),
        _ => {
            return Err(KnowledgeError::Inconsistent {
                table: TABLE,
                reason: "lineage kind and target must be set together".to_owned(),
            });
        }
    };
    Ok(PromotionReceipt {
        uid: uid_column(row, 0, TABLE)?,
        workspace: uid_column(row, 1, TABLE)?,
        work_note: uid_column(row, 2, TABLE)?,
        work_note_kind: WorkNoteKind::parse(&row.get::<_, String>(3)?)?,
        work_note_source_kind: SourceKind::parse(&row.get::<_, String>(4)?)?,
        work_note_fingerprint: row.get(5)?,
        target,
        request_fingerprint: row.get(8)?,
        evidence: PromotionEvidence {
            basis: PromotionBasis::parse(&row.get::<_, String>(9)?)?,
            provenance: Provenance {
                source_kind: SourceKind::parse(&row.get::<_, String>(10)?)?,
                locator: row.get(11)?,
                revision: row.get(12)?,
            },
        },
        lineage,
        created_at: row.get(15)?,
    })
}

// ----------------------------------------------------------- fingerprints

/// Ordered `(field, value)` pairs for [`db::fingerprint`]. An optional
/// value is `none` or `some:<value>`, so absent and empty never collide.
struct Fields(Vec<(&'static str, String)>);

impl Fields {
    fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.0.push((name, value.into()));
        self
    }

    fn with_opt(self, name: &'static str, value: Option<&str>) -> Self {
        self.with(
            name,
            value.map_or_else(|| "none".to_owned(), |v| format!("some:{v}")),
        )
    }

    fn with_scope(self, scope: &KnowledgeScope) -> Self {
        self.with("scope_kind", scope.kind().as_str())
            .with_opt("scope_key", scope.key())
    }

    fn finish(&self, tag: &str) -> String {
        let pairs: Vec<(&str, &str)> = self.0.iter().map(|(n, v)| (*n, v.as_str())).collect();
        db::fingerprint(tag, &pairs)
    }
}

/// Immutable evidence of the source note. Status, `updated_at` and the
/// promoted target change during promotion and are not included.
fn note_fingerprint(note: &WorkNote) -> String {
    Fields(Vec::new())
        .with("work_note", note.uid.to_string())
        .with("work_item", note.work_item.to_string())
        .with("kind", note.kind.as_str())
        .with("note_text", note.note_text.as_str())
        .with("source_kind", note.provenance.source_kind.as_str())
        .with_opt("source_ref", note.provenance.locator.as_deref())
        .with_opt("source_revision", note.provenance.revision.as_deref())
        .finish("knowledge-work-note-1")
}

/// The logical request: evidence, exact target, lineage. No timestamps.
fn request_fingerprint(request: &PromotionRequest) -> String {
    let evidence = &request.evidence;
    let fields = Fields(Vec::new())
        .with("work_note", request.work_note.to_string())
        .with("basis", evidence.basis.as_str())
        .with(
            "authority_source_kind",
            evidence.provenance.source_kind.as_str(),
        )
        .with_opt("authority_locator", evidence.provenance.locator.as_deref())
        .with_opt(
            "authority_revision",
            evidence.provenance.revision.as_deref(),
        )
        .with("target_kind", request.target.kind().as_str());
    let fields = match &request.target {
        PromotionTarget::Policy(policy) => fields
            .with_scope(&policy.scope)
            .with_opt("policy_key", policy.policy_key.as_deref())
            .with("title", policy.title.as_str())
            .with("rule_text", policy.rule_text.as_str())
            .with_opt(
                "structured_rule",
                policy
                    .structured_rule
                    .as_ref()
                    .map(canonical_json)
                    .as_deref(),
            )
            .with("protection_class", policy.protection_class.as_str())
            .with("priority_class", policy.priority_class.as_str()),
        PromotionTarget::Decision(decision) => fields
            .with_scope(&decision.scope)
            .with("topic", decision.topic.as_str())
            .with("chosen_summary", decision.chosen_summary.as_str())
            .with("rationale", decision.rationale.as_str()),
        PromotionTarget::ProjectState(state) => fields
            .with_scope(&state.scope)
            .with("key", state.key.as_str())
            .with("value_type", state.value.value_type().as_str())
            .with(
                "value",
                match &state.value {
                    TypedValue::Json(value) => canonical_json(value),
                    other => other.to_json(),
                },
            )
            .with("status", state.status.as_str()),
    };
    let lineage = request.target.lineage().map(PromotionLineage::parts);
    fields
        .with_opt("lineage_kind", lineage.map(|(kind, _)| kind.as_str()))
        .with_opt(
            "lineage_target",
            lineage
                .map(|(_, uid)| uid.iter().map(|b| format!("{b:02x}")).collect::<String>())
                .as_deref(),
        )
        .finish("knowledge-promotion-request-1")
}

/// JSON text with object keys sorted at every level. serde_json's map
/// keeps insertion order in this build (`preserve_order` is enabled by
/// feature unification), so `Value::to_string` alone is not canonical.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            let body: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::Value::from(key.as_str()),
                        canonical_json(value)
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", body.join(","))
        }
        scalar => scalar.to_string(),
    }
}

#[cfg(test)]
mod tests;
