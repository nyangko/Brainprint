//! #20 task 4 acceptance: WorkNote promotion authority, scope, lineage,
//! receipt-first retry and project-phase atomicity. Case numbers in
//! comments refer to the task 4 acceptance list in #20.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use brainprint_core::{DecisionId, PolicyId, ProjectId, WorkNoteId, WorkspaceId};
use rusqlite::{Connection, params};
use serde_json::json;

use super::*;
use crate::{
    db::{self, DbKind},
    generation::GenerationStore,
    init::init_workspace,
    knowledge::{
        ApplicabilityContext, DirtyObservation, GlobalKnowledgeStore, KnowledgeSources,
        NewWorkItem, NewWorkNote, ResolveRequest, StartObservation, WorkItemSourceKind,
        WorkRuntime, resolve,
    },
    paths::GlobalPaths,
    schema,
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-promotion-{label}-{}-{sequence}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("test directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One initialized Workspace that is its Project's project-home.
struct Fixture {
    _home: TestDir,
    _root: TestDir,
    global: GlobalPaths,
    project: ProjectId,
    workspace: WorkspaceId,
    paths: WorkspacePaths,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let root = TestDir::create(&format!("{label}-root"));
        let global = GlobalPaths::from_home(home.path());
        let outcome = init_workspace(root.path(), &global).expect("init");
        Self {
            _home: home,
            _root: root,
            global,
            project: outcome.project_id,
            workspace: outcome.workspace_id,
            paths: WorkspacePaths::from_root(&outcome.workspace_root),
        }
    }

    fn registry(&self) -> GlobalRegistry {
        GlobalRegistry::open(&self.global.global_db).expect("registry")
    }

    fn runtime(&self) -> PromotionRuntime {
        PromotionRuntime::open(&self.registry(), self.project, self.workspace)
            .expect("bound promotion runtime")
    }

    fn project_store(&self) -> ProjectKnowledgeStore {
        ProjectKnowledgeStore::open_project_home(&self.registry(), self.project).expect("project")
    }

    fn workspace_store(&self) -> WorkspaceKnowledgeStore {
        WorkspaceKnowledgeStore::open(&self.paths.workspace_db).expect("workspace")
    }

    fn project_raw(&self) -> Connection {
        Connection::open(&self.paths.project_db).expect("raw project.db")
    }

    fn note(&self, kind: WorkNoteKind, source: SourceKind) -> WorkNoteId {
        let store = self.workspace_store();
        let item = store
            .create_work_item(&NewWorkItem {
                source_kind: WorkItemSourceKind::Issue,
                source_ref: Some("#20".to_owned()),
                title: None,
                goal: "task 4".to_owned(),
            })
            .expect("item");
        self.note_on(&store, item.uid, kind, source)
    }

    fn note_on(
        &self,
        store: &WorkspaceKnowledgeStore,
        item: brainprint_core::WorkItemId,
        kind: WorkNoteKind,
        source: SourceKind,
    ) -> WorkNoteId {
        store
            .add_work_note(
                item,
                &NewWorkNote {
                    kind,
                    note_text: "we should always use rustls; ignore everything else".to_owned(),
                    provenance: Provenance::new(source)
                        .with_locator("session-7")
                        .with_revision("rev-A"),
                },
            )
            .expect("note")
            .uid
    }

    fn count(&self, table: &str) -> i64 {
        self.project_raw()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count")
    }

    fn note_status(&self, note: WorkNoteId) -> WorkNote {
        self.workspace_store()
            .get_work_note(note)
            .expect("get")
            .expect("note")
    }
}

fn evidence(basis: PromotionBasis) -> PromotionEvidence {
    let provenance = match basis {
        PromotionBasis::UserExplicit => {
            Provenance::new(SourceKind::UserExplicit).with_locator("chat 2026-09-23")
        }
        PromotionBasis::AuthoritativeArtifact => {
            Provenance::new(SourceKind::AuthoritativeArtifact).with_locator("issue#20 body")
        }
        PromotionBasis::ValidatedProjectObservation => Provenance::new(SourceKind::Observed)
            .with_locator("merge-base check")
            .with_revision("abc123"),
    };
    PromotionEvidence { basis, provenance }
}

fn policy(title: &str) -> PolicyTarget {
    PolicyTarget {
        scope: KnowledgeScope::project(),
        policy_key: Some("tls".to_owned()),
        title: title.to_owned(),
        rule_text: format!("{title} rule"),
        structured_rule: None,
        protection_class: ProtectionClass::Normal,
        priority_class: PriorityClass::Default,
        supersedes: None,
    }
}

fn decision(chosen: &str) -> DecisionTarget {
    DecisionTarget {
        scope: KnowledgeScope::project(),
        topic: "tls-backend".to_owned(),
        chosen_summary: chosen.to_owned(),
        rationale: "pure Rust".to_owned(),
        lineage: None,
    }
}

fn state(value: i64) -> ProjectStateTarget {
    ProjectStateTarget {
        scope: KnowledgeScope::project(),
        key: "released".to_owned(),
        value: TypedValue::Integer(value),
        status: ProjectStateStatus::Current,
    }
}

fn request(note: WorkNoteId, basis: PromotionBasis, target: PromotionTarget) -> PromotionRequest {
    PromotionRequest {
        work_note: note,
        evidence: evidence(basis),
        target,
    }
}

fn agent_reported_evidence(basis: PromotionBasis) -> PromotionEvidence {
    PromotionEvidence {
        basis,
        provenance: Provenance::new(SourceKind::AgentReported)
            .with_locator("agent")
            .with_revision("r"),
    }
}

fn decision_of(result: &PromotionResult) -> DecisionId {
    match result.receipt.target {
        PromotedItem::Decision(id) => id,
        other => panic!("expected a Decision, got {other:?}"),
    }
}

fn policy_of(result: &PromotionResult) -> PolicyId {
    match result.receipt.target {
        PromotedItem::Policy(id) => id,
        other => panic!("expected a Policy, got {other:?}"),
    }
}

fn state_of(result: &PromotionResult) -> ProjectStateId {
    match result.receipt.target {
        PromotedItem::ProjectState(id) => id,
        other => panic!("expected a Project State, got {other:?}"),
    }
}

fn resolve_project(fixture: &Fixture, topics: &[&str]) -> crate::knowledge::ResolvedKnowledge {
    let global = GlobalKnowledgeStore::open(&fixture.global.global_db).expect("global");
    let project = fixture.project_store();
    let mut request = ResolveRequest::new(ApplicabilityContext::base(None));
    request.decision_topics = topics.iter().map(|t| (*t).to_owned()).collect();
    resolve(
        &KnowledgeSources {
            global: &global,
            project: &project,
            workspace: None,
        },
        &request,
    )
    .expect("resolve")
}

// ============================================ before promotion (1, 2, 58)

#[test]
fn unpromoted_notes_are_not_project_truth() {
    let fixture = Fixture::new("unpromoted");
    fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    fixture.note(WorkNoteKind::Observation, SourceKind::Observed);
    for table in ["policy", "decision", "project_state", "knowledge_promotion"] {
        assert_eq!(fixture.count(table), 0, "{table}");
    }
    let resolved = resolve_project(&fixture, &["tls-backend"]);
    assert!(resolved.applied_policies.is_empty());
    assert!(resolved.active_decisions.is_empty());
}

// ================================= Decision authority (3-6, 20, 22, 24-29)

#[test]
fn a_proposal_becomes_a_decision_only_by_user_or_artifact_authority() {
    let fixture = Fixture::new("decision-authority");
    let runtime = fixture.runtime();
    for basis in [
        PromotionBasis::UserExplicit,
        PromotionBasis::AuthoritativeArtifact,
    ] {
        let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
        let before = fixture.note_status(note);
        let result = runtime
            .promote(&request(
                note,
                basis,
                PromotionTarget::Decision(decision("rustls")),
            ))
            .expect("promote");
        assert_eq!(result.outcome, PromotionOutcome::Created);
        let created = fixture
            .project_store()
            .get_decision(decision_of(&result))
            .expect("get")
            .expect("target exists");
        // 20: the payload is the request's; the note text was not parsed.
        assert_eq!(created.chosen_summary, "rustls");
        assert_eq!(created.topic, "tls-backend");
        // 22: target provenance is the promotion evidence.
        assert_eq!(created.provenance, evidence(basis).provenance);
        assert_eq!(created.status, DecisionStatus::Active);
        // 24-26: the note keeps its own provenance and names the target.
        let after = fixture.note_status(note);
        assert_eq!(after.provenance, before.provenance);
        assert_eq!(after.provenance.source_kind, SourceKind::AgentReported);
        assert_eq!(after.status, WorkNoteStatus::Promoted);
        assert_eq!(after.promoted_item, Some(result.receipt.target));
        // 27-29: the receipt holds both sides and the request identity.
        let receipt = runtime.receipt(note).expect("read").expect("receipt");
        assert_eq!(receipt, result.receipt);
        assert_eq!(receipt.workspace, fixture.workspace);
        assert_eq!(receipt.work_note, note);
        assert_eq!(receipt.work_note_kind, WorkNoteKind::Proposal);
        assert_eq!(receipt.work_note_source_kind, SourceKind::AgentReported);
        assert!(
            receipt
                .work_note_fingerprint
                .starts_with("knowledge-work-note-1:")
        );
        assert!(
            receipt
                .request_fingerprint
                .starts_with("knowledge-promotion-request-1:")
        );
        assert_eq!(receipt.evidence, evidence(basis));
        assert_eq!(receipt.lineage, None);
    }

    // 5, 6: AGENT_REPORTED authority, or a validated observation, never.
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    for basis in [
        PromotionBasis::UserExplicit,
        PromotionBasis::AuthoritativeArtifact,
        PromotionBasis::ValidatedProjectObservation,
    ] {
        assert!(matches!(
            runtime.promote(&PromotionRequest {
                work_note: note,
                evidence: agent_reported_evidence(basis),
                target: PromotionTarget::Decision(decision("x")),
            }),
            Err(PromotionError::InvalidEvidence(_))
        ));
    }
    assert!(matches!(
        runtime.promote(&request(
            note,
            PromotionBasis::ValidatedProjectObservation,
            PromotionTarget::Decision(decision("x"))
        )),
        Err(PromotionError::AuthorityNotAllowed { .. })
    ));
    assert_eq!(fixture.count("decision"), 2);
    assert_eq!(fixture.note_status(note).status, WorkNoteStatus::Open);
}

#[test]
fn evidence_rules_are_exact_not_ranked() {
    let missing_locator = PromotionEvidence {
        basis: PromotionBasis::AuthoritativeArtifact,
        provenance: Provenance::new(SourceKind::AuthoritativeArtifact),
    };
    assert!(missing_locator.validate().is_err());
    let missing_revision = PromotionEvidence {
        basis: PromotionBasis::ValidatedProjectObservation,
        provenance: Provenance::new(SourceKind::Observed).with_locator("x"),
    };
    assert!(missing_revision.validate().is_err());
    // A "stronger" source kind does not stand in for the named one.
    let user_as_artifact = PromotionEvidence {
        basis: PromotionBasis::AuthoritativeArtifact,
        provenance: Provenance::new(SourceKind::UserExplicit).with_locator("x"),
    };
    assert!(user_as_artifact.validate().is_err());
    assert!(
        PromotionEvidence {
            basis: PromotionBasis::UserExplicit,
            provenance: Provenance::new(SourceKind::UserExplicit),
        }
        .validate()
        .is_ok(),
        "a user statement needs no locator"
    );
}

// ================================ Policy authority (7-9, 21, 53, 57)

#[test]
fn a_proposal_becomes_a_policy_only_by_user_or_artifact_authority() {
    let fixture = Fixture::new("policy-authority");
    let runtime = fixture.runtime();
    for basis in [
        PromotionBasis::UserExplicit,
        PromotionBasis::AuthoritativeArtifact,
    ] {
        let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
        let result = runtime
            .promote(&request(
                note,
                basis,
                PromotionTarget::Policy(PolicyTarget {
                    policy_key: Some(format!("key-{basis}")),
                    ..policy("use rustls")
                }),
            ))
            .expect("promote");
        let created = fixture
            .project_store()
            .get_policy(policy_of(&result))
            .expect("get")
            .expect("policy");
        assert_eq!(created.provenance, evidence(basis).provenance);
        assert_eq!(created.rule_text, "use rustls rule");
    }
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    assert!(matches!(
        runtime.promote(&PromotionRequest {
            work_note: note,
            evidence: agent_reported_evidence(PromotionBasis::UserExplicit),
            target: PromotionTarget::Policy(policy("x")),
        }),
        Err(PromotionError::InvalidEvidence(_))
    ));
    assert_eq!(fixture.count("policy"), 2);
}

#[test]
fn a_protected_policy_passes_the_same_gate_and_the_resolver_accepts_it() {
    let fixture = Fixture::new("protected");
    let runtime = fixture.runtime();
    let protected = PolicyTarget {
        protection_class: ProtectionClass::ProtectedSecurity,
        policy_key: Some("secrets".to_owned()),
        ..policy("never log tokens")
    };
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    assert!(matches!(
        runtime.promote(&PromotionRequest {
            work_note: note,
            evidence: agent_reported_evidence(PromotionBasis::UserExplicit),
            target: PromotionTarget::Policy(protected.clone()),
        }),
        Err(PromotionError::InvalidEvidence(_))
    ));
    assert!(matches!(
        runtime.promote(&request(
            note,
            PromotionBasis::ValidatedProjectObservation,
            PromotionTarget::Policy(protected.clone())
        )),
        Err(PromotionError::AuthorityNotAllowed { .. })
    ));
    let result = runtime
        .promote(&request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Policy(protected),
        ))
        .expect("user adopts the agent's protected rule");

    // 57: the resolver reads it through its existing provenance gate.
    let resolved = resolve_project(&fixture, &[]);
    let uids: Vec<PolicyId> = resolved
        .protected_constraints
        .iter()
        .map(|entry| entry.item.uid)
        .collect();
    assert_eq!(uids, vec![policy_of(&result)]);
    assert!(resolved.conflicts.is_empty());
}

// ============================ Project State authority (10-13, 23)

#[test]
fn an_observation_becomes_project_state_only_with_project_level_evidence() {
    let fixture = Fixture::new("state-authority");
    let runtime = fixture.runtime();
    for (index, basis) in [
        PromotionBasis::UserExplicit,
        PromotionBasis::AuthoritativeArtifact,
        PromotionBasis::ValidatedProjectObservation,
    ]
    .into_iter()
    .enumerate()
    {
        let note = fixture.note(WorkNoteKind::Observation, SourceKind::Observed);
        let result = runtime
            .promote(&request(
                note,
                basis,
                PromotionTarget::ProjectState(ProjectStateTarget {
                    key: format!("fact-{index}"),
                    ..state(1)
                }),
            ))
            .expect("promote");
        let stored = fixture
            .project_store()
            .get_project_state_by_uid(state_of(&result))
            .expect("get")
            .expect("state");
        assert_eq!(stored.provenance, evidence(basis).provenance);
        assert_eq!(stored.value, TypedValue::Integer(1));
    }
    // 13: a validated observation needs an OBSERVED source note.
    let reported = fixture.note(WorkNoteKind::Observation, SourceKind::AgentReported);
    assert!(matches!(
        runtime.promote(&request(
            reported,
            PromotionBasis::ValidatedProjectObservation,
            PromotionTarget::ProjectState(state(9))
        )),
        Err(PromotionError::AuthorityNotAllowed { .. })
    ));
    assert_eq!(fixture.count("project_state"), 3);
}

// ===================================== category / status (14-19, 55, 56)

#[test]
fn only_the_three_p0_category_transitions_exist() {
    let fixture = Fixture::new("category");
    let runtime = fixture.runtime();
    let cases = [
        (
            WorkNoteKind::Proposal,
            PromotionTarget::ProjectState(state(1)),
        ),
        (
            WorkNoteKind::Observation,
            PromotionTarget::Decision(decision("x")),
        ),
        (
            WorkNoteKind::Observation,
            PromotionTarget::Policy(policy("x")),
        ),
        (
            WorkNoteKind::OpenQuestion,
            PromotionTarget::Decision(decision("x")),
        ),
        (
            WorkNoteKind::OpenQuestion,
            PromotionTarget::Policy(policy("x")),
        ),
        (
            WorkNoteKind::OpenQuestion,
            PromotionTarget::ProjectState(state(1)),
        ),
    ];
    for (kind, target) in cases {
        let note = fixture.note(kind, SourceKind::Observed);
        assert!(
            matches!(
                runtime.promote(&PromotionRequest {
                    work_note: note,
                    evidence: evidence(PromotionBasis::UserExplicit),
                    target,
                }),
                Err(PromotionError::CategoryNotAllowed { .. })
            ),
            "{kind}"
        );
        assert_eq!(fixture.note_status(note).status, WorkNoteStatus::Open);
    }
    for table in ["policy", "decision", "project_state", "knowledge_promotion"] {
        assert_eq!(fixture.count(table), 0, "{table}");
    }
}

#[test]
fn closed_notes_are_not_promoted_and_promoted_notes_stay_promoted() {
    let fixture = Fixture::new("status");
    let runtime = fixture.runtime();
    let store = fixture.workspace_store();
    for closed in [WorkNoteStatus::Resolved, WorkNoteStatus::Discarded] {
        let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
        store.set_work_note_status(note, closed).expect("close");
        assert!(matches!(
            runtime.promote(&request(
                note,
                PromotionBasis::UserExplicit,
                PromotionTarget::Decision(decision("x"))
            )),
            Err(PromotionError::NoteNotPromotable(status)) if status == closed
        ));
    }
    assert_eq!(fixture.count("decision"), 0);

    // 55: the ordinary setter still has no PROMOTED.
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    assert!(
        store
            .set_work_note_status(note, WorkNoteStatus::Promoted)
            .is_err()
    );
    // 56: once promoted, the note cannot be resolved or discarded.
    runtime
        .promote(&request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(decision("x")),
        ))
        .expect("promote");
    for next in [WorkNoteStatus::Resolved, WorkNoteStatus::Discarded] {
        assert!(store.set_work_note_status(note, next).is_err());
    }
    // The marker is not rewritten to another target either.
    assert!(
        store
            .mark_work_note_promoted(note, PromotedItem::Decision(DecisionId::generate()))
            .is_err()
    );
    assert_eq!(fixture.note_status(note).status, WorkNoteStatus::Promoted);
}

// ========================================= retry (30-36, 56 scenario)

#[test]
fn a_repeated_promotion_is_idempotent() {
    let fixture = Fixture::new("repeat");
    let runtime = fixture.runtime();
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    let req = request(
        note,
        PromotionBasis::UserExplicit,
        PromotionTarget::Decision(decision("rustls")),
    );
    let first = runtime.promote(&req).expect("first");
    let second = runtime.promote(&req).expect("second");
    assert_eq!(second.outcome, PromotionOutcome::AlreadyPromoted);
    assert_eq!(second.receipt, first.receipt);
    assert_eq!(fixture.count("decision"), 1);
    assert_eq!(fixture.count("knowledge_promotion"), 1);
}

#[test]
fn an_interrupted_promotion_is_reconciled_without_a_second_target() {
    // The mandatory cross-DB scenario: project phase committed, the process
    // stopped before the note was marked, then a fresh runtime retries.
    let fixture = Fixture::new("interrupted");
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    let req = request(
        note,
        PromotionBasis::UserExplicit,
        PromotionTarget::Decision(decision("rustls")),
    );
    let committed = {
        let runtime = fixture.runtime();
        let checked = runtime.check(&req).expect("gates");
        match runtime.project_phase(&checked).expect("project phase") {
            ProjectPhase::Committed(receipt) => receipt,
            ProjectPhase::AlreadyReceipted(_) => panic!("fresh note has no receipt"),
        }
    };
    assert_eq!(fixture.note_status(note).status, WorkNoteStatus::Open);
    assert_eq!(fixture.count("decision"), 1);

    let runtime = fixture.runtime();
    let reconciled = runtime.promote(&req).expect("retry");
    assert_eq!(reconciled.outcome, PromotionOutcome::Reconciled);
    assert_eq!(reconciled.receipt, committed, "same receipt, same target");
    assert_eq!(fixture.count("decision"), 1, "no duplicate Decision");
    assert_eq!(
        fixture.note_status(note).promoted_item,
        Some(committed.target)
    );

    let again = runtime.promote(&req).expect("retry again");
    assert_eq!(again.outcome, PromotionOutcome::AlreadyPromoted);
    assert_eq!(again.receipt, committed);
    assert_eq!(fixture.count("decision"), 1);
    assert_eq!(fixture.count("knowledge_promotion"), 1);
}

#[test]
fn a_different_request_for_a_promoted_note_is_a_conflict() {
    let fixture = Fixture::new("conflict");
    let runtime = fixture.runtime();
    let old = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    let old_decision = decision_of(
        &runtime
            .promote(&request(
                old,
                PromotionBasis::UserExplicit,
                PromotionTarget::Decision(decision("openssl")),
            ))
            .expect("old"),
    );
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    let original = request(
        note,
        PromotionBasis::UserExplicit,
        PromotionTarget::Decision(decision("rustls")),
    );
    let promoted = runtime.promote(&original).expect("promote");
    let changed = [
        // 33: payload.
        request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(decision("native-tls")),
        ),
        // 34: authority.
        request(
            note,
            PromotionBasis::AuthoritativeArtifact,
            PromotionTarget::Decision(decision("rustls")),
        ),
        // 35: lineage.
        request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(DecisionTarget {
                lineage: Some(DecisionLineageAction::Supersedes(old_decision)),
                ..decision("rustls")
            }),
        ),
    ];
    for retry in changed {
        assert!(matches!(
            runtime.promote(&retry),
            Err(PromotionError::IdempotencyConflict(id)) if id == promoted.receipt.uid
        ));
    }
    let project = fixture.project_store();
    assert_eq!(
        project
            .get_decision(old_decision)
            .expect("get")
            .expect("old")
            .status,
        DecisionStatus::Active,
        "the conflicting lineage was not applied"
    );
    assert_eq!(
        project
            .get_decision(decision_of(&promoted))
            .expect("get")
            .expect("d")
            .chosen_summary,
        "rustls"
    );
    assert_eq!(fixture.count("decision"), 2);
}

#[test]
fn disagreeing_note_and_receipt_are_reported_not_repaired() {
    // 36, and a vanished target (provenance gap) is reported, not deleted
    // around.
    let fixture = Fixture::new("corrupt");
    let runtime = fixture.runtime();
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    let req = request(
        note,
        PromotionBasis::UserExplicit,
        PromotionTarget::Decision(decision("rustls")),
    );
    let promoted = runtime.promote(&req).expect("promote");
    let raw = Connection::open(&fixture.paths.workspace_db).expect("raw");
    raw.execute(
        "UPDATE work_note SET promoted_item_uid = ?1 WHERE uid = ?2",
        params![
            DecisionId::generate().to_bytes().to_vec(),
            note.to_bytes().to_vec()
        ],
    )
    .expect("tamper");
    assert!(matches!(
        runtime.promote(&req),
        Err(PromotionError::CorruptPromotionState(_))
    ));
    fixture
        .project_raw()
        .execute(
            "DELETE FROM decision WHERE uid = ?1",
            params![decision_of(&promoted).to_bytes().to_vec()],
        )
        .expect("remove target");
    assert!(matches!(
        runtime.promote(&req),
        Err(PromotionError::CorruptPromotionState(_))
    ));
    assert_eq!(fixture.count("knowledge_promotion"), 1, "receipt kept");
}

// ================================================ Policy lineage (37-39, 47)

#[test]
fn policy_supersession_is_explicit_and_exact() {
    let fixture = Fixture::new("policy-lineage");
    let runtime = fixture.runtime();
    let project = fixture.project_store();
    let old = policy_of(
        &runtime
            .promote(&request(
                fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported),
                PromotionBasis::UserExplicit,
                PromotionTarget::Policy(policy("v1")),
            ))
            .expect("old"),
    );
    // Without a lineage request, a same-key Policy retires nothing.
    let sibling = runtime
        .promote(&request(
            fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported),
            PromotionBasis::UserExplicit,
            PromotionTarget::Policy(policy("sibling")),
        ))
        .expect("sibling");
    assert_eq!(
        project.get_policy(old).expect("g").expect("p").status,
        PolicyStatus::Active
    );
    assert_eq!(sibling.receipt.lineage, None);

    // 38, 39: wrong key (incl. keyed vs unkeyed), wrong scope, missing.
    let mismatches = [
        PolicyTarget {
            policy_key: Some("other".to_owned()),
            supersedes: Some(old),
            ..policy("v2")
        },
        PolicyTarget {
            policy_key: None,
            supersedes: Some(old),
            ..policy("v2")
        },
        PolicyTarget {
            scope: KnowledgeScope::workspace(fixture.workspace),
            supersedes: Some(old),
            ..policy("v2")
        },
        PolicyTarget {
            supersedes: Some(PolicyId::generate()),
            ..policy("v2")
        },
    ];
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    let policies = fixture.count("policy");
    for target in mismatches {
        assert!(matches!(
            runtime.promote(&request(
                note,
                PromotionBasis::UserExplicit,
                PromotionTarget::Policy(target)
            )),
            Err(PromotionError::InvalidTarget(_))
        ));
    }
    assert_eq!(fixture.count("policy"), policies, "no replacement row");
    assert_eq!(fixture.count("policy_link"), 0);
    assert_eq!(
        project.get_policy(old).expect("g").expect("p").status,
        PolicyStatus::Active
    );
    assert_eq!(fixture.note_status(note).status, WorkNoteStatus::Open);

    // 37: the explicit, exact supersession.
    let replacing = request(
        note,
        PromotionBasis::UserExplicit,
        PromotionTarget::Policy(PolicyTarget {
            supersedes: Some(old),
            ..policy("v2")
        }),
    );
    let replacement = runtime.promote(&replacing).expect("supersede");
    let new = policy_of(&replacement);
    assert_eq!(
        replacement.receipt.lineage,
        Some(PromotionLineage::PolicySupersedes(old))
    );
    assert_eq!(
        project.get_policy(old).expect("g").expect("p").status,
        PolicyStatus::Superseded
    );
    assert_eq!(
        project.policy_lineage(new).expect("lineage").supersedes,
        vec![old]
    );

    // An inactive old Policy cannot be superseded again.
    assert!(matches!(
        runtime.promote(&request(
            fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported),
            PromotionBasis::UserExplicit,
            PromotionTarget::Policy(PolicyTarget {
                supersedes: Some(old),
                ..policy("v3")
            }),
        )),
        Err(PromotionError::InvalidTarget(_))
    ));

    // 47: the promoted Policy is later superseded; retrying the original
    // promotion recognizes it and does not reactivate it.
    let later = runtime
        .promote(&request(
            fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported),
            PromotionBasis::UserExplicit,
            PromotionTarget::Policy(PolicyTarget {
                supersedes: Some(new),
                ..policy("v3")
            }),
        ))
        .expect("later");
    let retry = runtime.promote(&replacing).expect("retry");
    assert_eq!(retry.outcome, PromotionOutcome::AlreadyPromoted);
    assert_eq!(retry.receipt.target, PromotedItem::Policy(new));
    assert_eq!(
        project.get_policy(new).expect("g").expect("p").status,
        PolicyStatus::Superseded
    );
    assert_eq!(
        project
            .get_policy(policy_of(&later))
            .expect("g")
            .expect("p")
            .status,
        PolicyStatus::Active
    );
}

// ============================================ Decision lineage (40-43, 46)

#[test]
fn decision_supersession_and_reversal_are_explicit_and_exact() {
    let fixture = Fixture::new("decision-lineage");
    let runtime = fixture.runtime();
    let project = fixture.project_store();
    let promote = |target: DecisionTarget| {
        runtime.promote(&request(
            fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported),
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(target),
        ))
    };
    let first = decision_of(&promote(decision("openssl")).expect("first"));

    // 42, 43: different topic / scope / unknown target change nothing.
    let decisions = fixture.count("decision");
    for target in [
        DecisionTarget {
            topic: "tls".to_owned(),
            lineage: Some(DecisionLineageAction::Supersedes(first)),
            ..decision("rustls")
        },
        DecisionTarget {
            scope: KnowledgeScope::workspace(fixture.workspace),
            lineage: Some(DecisionLineageAction::Reverses(first)),
            ..decision("rustls")
        },
        DecisionTarget {
            lineage: Some(DecisionLineageAction::Supersedes(DecisionId::generate())),
            ..decision("rustls")
        },
    ] {
        assert!(matches!(
            promote(target),
            Err(PromotionError::InvalidTarget(_))
        ));
    }
    assert_eq!(fixture.count("decision"), decisions);
    assert_eq!(fixture.count("decision_link"), 0);
    assert_eq!(
        project.get_decision(first).expect("g").expect("d").status,
        DecisionStatus::Active
    );

    // 40: SUPERSEDES.
    let supersede_note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    let superseding = request(
        supersede_note,
        PromotionBasis::UserExplicit,
        PromotionTarget::Decision(DecisionTarget {
            lineage: Some(DecisionLineageAction::Supersedes(first)),
            ..decision("rustls")
        }),
    );
    let second = runtime.promote(&superseding).expect("supersede");
    let second_id = decision_of(&second);
    assert_eq!(
        second.receipt.lineage,
        Some(PromotionLineage::DecisionSupersedes(first))
    );
    assert_eq!(
        project.get_decision(first).expect("g").expect("d").status,
        DecisionStatus::Superseded
    );

    // 41: REVERSES.
    let third = promote(DecisionTarget {
        lineage: Some(DecisionLineageAction::Reverses(second_id)),
        ..decision("back to openssl")
    })
    .expect("reverse");
    assert_eq!(
        third.receipt.lineage,
        Some(PromotionLineage::DecisionReverses(second_id))
    );
    assert_eq!(
        project
            .get_decision(second_id)
            .expect("g")
            .expect("d")
            .status,
        DecisionStatus::Reversed
    );
    let lineage = project
        .decision_lineage(decision_of(&third))
        .expect("lineage");
    assert_eq!(lineage.outgoing.len(), 1);
    assert_eq!(lineage.outgoing[0].kind, DecisionLinkKind::Reverses);

    // 46: retrying the later-reversed promotion does not reactivate it.
    let retry = runtime.promote(&superseding).expect("retry");
    assert_eq!(retry.outcome, PromotionOutcome::AlreadyPromoted);
    assert_eq!(
        project
            .get_decision(second_id)
            .expect("g")
            .expect("d")
            .status,
        DecisionStatus::Reversed
    );

    // 57: the resolver sees exactly the active one.
    let resolved = resolve_project(&fixture, &["tls-backend"]);
    let active: Vec<DecisionId> = resolved
        .active_decisions
        .iter()
        .map(|d| d.item.uid)
        .collect();
    assert_eq!(active, vec![decision_of(&third)]);
}

// ================================================ Project State (44, 45)

#[test]
fn project_state_upserts_in_place_and_an_old_retry_never_reverts_it() {
    let fixture = Fixture::new("state-retry");
    let runtime = fixture.runtime();
    let first_note = fixture.note(WorkNoteKind::Observation, SourceKind::Observed);
    let first = request(
        first_note,
        PromotionBasis::ValidatedProjectObservation,
        PromotionTarget::ProjectState(state(1)),
    );
    let promoted = runtime.promote(&first).expect("foo=1");
    let uid = state_of(&promoted);

    // 44: a second promotion of the same (scope, key) keeps the uid.
    let second = runtime
        .promote(&request(
            fixture.note(WorkNoteKind::Observation, SourceKind::Observed),
            PromotionBasis::UserExplicit,
            PromotionTarget::ProjectState(state(2)),
        ))
        .expect("foo=2");
    assert_eq!(state_of(&second), uid);
    let project = fixture.project_store();
    let current = || {
        project
            .get_project_state_by_uid(uid)
            .expect("get")
            .expect("state")
    };
    assert_eq!(current().value, TypedValue::Integer(2));
    assert_eq!(fixture.count("project_state"), 1);

    // 45: the first promotion's note was left OPEN (as if the process
    // stopped after the project phase); retrying it only reconciles.
    Connection::open(&fixture.paths.workspace_db)
        .expect("raw")
        .execute(
            "UPDATE work_note SET status = 'OPEN', promoted_item_kind = NULL, \
             promoted_item_uid = NULL WHERE uid = ?1",
            params![first_note.to_bytes().to_vec()],
        )
        .expect("simulate the unmarked note");
    let retry = runtime.promote(&first).expect("retry");
    assert_eq!(retry.outcome, PromotionOutcome::Reconciled);
    assert_eq!(current().value, TypedValue::Integer(2), "not re-applied");
    assert_eq!(
        current().provenance,
        evidence(PromotionBasis::UserExplicit).provenance
    );
    let again = runtime.promote(&first).expect("again");
    assert_eq!(again.outcome, PromotionOutcome::AlreadyPromoted);
    assert_eq!(current().value, TypedValue::Integer(2));
}

// ============================================== scope safety (48-50)

#[test]
fn a_note_writes_only_its_own_workspace_scope_and_never_workspace_state() {
    let fixture = Fixture::new("scope");
    let runtime = fixture.runtime();
    let other = KnowledgeScope::workspace(WorkspaceId::generate());
    let note = fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported);
    for target in [
        PromotionTarget::Policy(PolicyTarget {
            scope: other.clone(),
            ..policy("x")
        }),
        PromotionTarget::Decision(DecisionTarget {
            scope: other.clone(),
            ..decision("x")
        }),
        PromotionTarget::Policy(PolicyTarget {
            scope: KnowledgeScope::global(),
            ..policy("x")
        }),
    ] {
        assert!(matches!(
            runtime.promote(&request(note, PromotionBasis::UserExplicit, target)),
            Err(PromotionError::InvalidTarget(_))
        ));
    }
    let observation = fixture.note(WorkNoteKind::Observation, SourceKind::Observed);
    assert!(matches!(
        runtime.promote(&request(
            observation,
            PromotionBasis::UserExplicit,
            PromotionTarget::ProjectState(ProjectStateTarget {
                scope: KnowledgeScope::workspace(fixture.workspace),
                ..state(1)
            })
        )),
        Err(PromotionError::InvalidTarget(_))
    ));
    assert_eq!(fixture.count("policy") + fixture.count("decision"), 0);

    // Its own WORKSPACE scope is fine.
    let own = runtime
        .promote(&request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(DecisionTarget {
                scope: KnowledgeScope::workspace(fixture.workspace),
                ..decision("x")
            }),
        ))
        .expect("own workspace scope");
    assert_eq!(
        fixture
            .project_store()
            .get_decision(decision_of(&own))
            .expect("g")
            .expect("d")
            .scope,
        KnowledgeScope::workspace(fixture.workspace)
    );
}

// ================================================ binding (51-53)

#[test]
fn the_runtime_binds_only_a_registered_workspace_of_its_project() {
    let a = Fixture::new("binding-a");
    let registry = a.registry();
    // 51: a Project the Workspace does not belong to.
    let b_root = TestDir::create("binding-b-root");
    let b = init_workspace(b_root.path(), &a.global).expect("second project");
    assert_ne!(b.project_id, a.project);
    assert!(matches!(
        PromotionRuntime::open(&registry, b.project_id, a.workspace),
        Err(PromotionError::WorkspaceNotInProject { .. })
    ));
    // 52: an unregistered WorkspaceID.
    assert!(matches!(
        PromotionRuntime::open(&registry, a.project, WorkspaceId::generate()),
        Err(PromotionError::UnknownWorkspace(_))
    ));
    // workspace.db bound to another Workspace.
    let raw = Connection::open(&a.paths.workspace_db).expect("raw");
    raw.execute(
        "UPDATE db_meta SET workspace_uid = ?1 WHERE id = 0",
        params![WorkspaceId::generate().to_bytes().to_vec()],
    )
    .expect("rebind");
    assert!(matches!(
        PromotionRuntime::open(&registry, a.project, a.workspace),
        Err(PromotionError::WorkspaceDbMismatch { .. })
    ));
    raw.execute(
        "UPDATE db_meta SET workspace_uid = ?1 WHERE id = 0",
        params![a.workspace.to_bytes().to_vec()],
    )
    .expect("restore");
    // 53: the project-home file bound to another Project, then missing.
    a.project_raw()
        .execute(
            "UPDATE db_meta SET project_uid = ?1 WHERE id = 0",
            params![ProjectId::generate().to_bytes().to_vec()],
        )
        .expect("rebind project.db");
    assert!(matches!(
        PromotionRuntime::open(&registry, a.project, a.workspace),
        Err(PromotionError::Knowledge(
            KnowledgeError::ProjectIdentityMismatch { .. }
        ))
    ));
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", a.paths.project_db.display()));
    }
    assert!(matches!(
        PromotionRuntime::open(&registry, a.project, a.workspace),
        Err(PromotionError::Knowledge(
            KnowledgeError::ProjectHomeMissing { .. }
        ))
    ));
    assert!(!a.paths.project_db.exists(), "never created");
}

// ============================================== rollback (54, 57 scenario)

/// Test-only: make the receipt insert -- the last write of the project
/// phase -- fail, after target and lineage were written in the same
/// transaction.
fn fail_receipt_inserts(fixture: &Fixture) {
    fixture
        .project_raw()
        .execute_batch(
            "CREATE TRIGGER fail_receipt BEFORE INSERT ON knowledge_promotion \
             BEGIN SELECT RAISE(ABORT, 'receipt phase failed'); END;",
        )
        .expect("trigger");
}

#[test]
fn a_failed_receipt_rolls_back_target_and_lineage() {
    let fixture = Fixture::new("rollback");
    let runtime = fixture.runtime();
    let project = fixture.project_store();
    let old_policy = policy_of(
        &runtime
            .promote(&request(
                fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported),
                PromotionBasis::UserExplicit,
                PromotionTarget::Policy(policy("v1")),
            ))
            .expect("old policy"),
    );
    let old_decision = decision_of(
        &runtime
            .promote(&request(
                fixture.note(WorkNoteKind::Proposal, SourceKind::AgentReported),
                PromotionBasis::UserExplicit,
                PromotionTarget::Decision(decision("openssl")),
            ))
            .expect("old decision"),
    );
    runtime
        .promote(&request(
            fixture.note(WorkNoteKind::Observation, SourceKind::Observed),
            PromotionBasis::UserExplicit,
            PromotionTarget::ProjectState(state(1)),
        ))
        .expect("state");
    let counts = |fixture: &Fixture| {
        [
            "policy",
            "policy_link",
            "decision",
            "decision_link",
            "knowledge_promotion",
        ]
        .map(|table| fixture.count(table))
    };
    let before = counts(&fixture);
    fail_receipt_inserts(&fixture);

    let attempts = [
        (
            WorkNoteKind::Proposal,
            PromotionTarget::Policy(PolicyTarget {
                supersedes: Some(old_policy),
                ..policy("v2")
            }),
        ),
        (
            WorkNoteKind::Proposal,
            PromotionTarget::Decision(DecisionTarget {
                lineage: Some(DecisionLineageAction::Reverses(old_decision)),
                ..decision("rustls")
            }),
        ),
        (
            WorkNoteKind::Observation,
            PromotionTarget::ProjectState(state(2)),
        ),
    ];
    for (kind, target) in attempts {
        let note = fixture.note(kind, SourceKind::Observed);
        assert!(
            runtime
                .promote(&request(note, PromotionBasis::UserExplicit, target))
                .is_err()
        );
        assert_eq!(fixture.note_status(note).status, WorkNoteStatus::Open);
    }
    assert_eq!(
        counts(&fixture),
        before,
        "no row of any failed attempt remains"
    );
    assert_eq!(
        project
            .get_policy(old_policy)
            .expect("g")
            .expect("p")
            .status,
        PolicyStatus::Active
    );
    assert_eq!(
        project
            .get_decision(old_decision)
            .expect("g")
            .expect("d")
            .status,
        DecisionStatus::Active
    );
    assert_eq!(
        project
            .get_project_state("released", &KnowledgeScope::project())
            .expect("get")
            .expect("state")
            .value,
        TypedValue::Integer(1),
        "no partial state update"
    );
}

// ============================================== fingerprints (task 4 §55)

#[test]
fn request_fingerprints_follow_the_logical_request_only() {
    let note = WorkNoteId::generate();
    let rule = |value: serde_json::Value| {
        request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Policy(PolicyTarget {
                structured_rule: Some(value),
                ..policy("p")
            }),
        )
    };
    let mut forward = serde_json::Map::new();
    forward.insert("a".to_owned(), json!(1));
    forward.insert("b".to_owned(), json!({"x": 1, "y": [1, {"k": 2, "j": 3}]}));
    let mut backward = serde_json::Map::new();
    backward.insert("b".to_owned(), json!({"y": [1, {"j": 3, "k": 2}], "x": 1}));
    backward.insert("a".to_owned(), json!(1));
    assert_eq!(
        request_fingerprint(&rule(serde_json::Value::Object(forward))),
        request_fingerprint(&rule(serde_json::Value::Object(backward))),
        "object key order is not part of the request"
    );

    let base = request(
        note,
        PromotionBasis::UserExplicit,
        PromotionTarget::Decision(decision("d")),
    );
    assert_eq!(
        request_fingerprint(&base),
        request_fingerprint(&base.clone())
    );
    let old = DecisionId::generate();
    let variants = [
        request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(decision("other")),
        ),
        request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(DecisionTarget {
                scope: KnowledgeScope::workspace(WorkspaceId::generate()),
                ..decision("d")
            }),
        ),
        request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(DecisionTarget {
                topic: "t2".to_owned(),
                ..decision("d")
            }),
        ),
        request(
            note,
            PromotionBasis::AuthoritativeArtifact,
            PromotionTarget::Decision(decision("d")),
        ),
        request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(DecisionTarget {
                lineage: Some(DecisionLineageAction::Supersedes(old)),
                ..decision("d")
            }),
        ),
        request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(DecisionTarget {
                lineage: Some(DecisionLineageAction::Reverses(old)),
                ..decision("d")
            }),
        ),
        request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(DecisionTarget {
                lineage: Some(DecisionLineageAction::Supersedes(DecisionId::generate())),
                ..decision("d")
            }),
        ),
    ];
    for variant in &variants {
        assert_ne!(
            request_fingerprint(variant),
            request_fingerprint(&base),
            "{variant:?}"
        );
    }
    let value = |v: TypedValue| {
        request_fingerprint(&request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::ProjectState(ProjectStateTarget {
                value: v,
                ..state(0)
            }),
        ))
    };
    assert_ne!(value(TypedValue::Integer(1)), value(TypedValue::Integer(2)));
    assert_ne!(
        value(TypedValue::Integer(1)),
        value(TypedValue::Text("1".to_owned()))
    );
    assert_eq!(
        value(TypedValue::Json(json!({"a": 1, "b": 2}))),
        value(TypedValue::Json(
            serde_json::from_str(r#"{"b":2,"a":1}"#).expect("json")
        ))
    );
}

// ============================================ Task 3 unaffected (62)

#[test]
fn promotion_leaves_the_work_items_working_state_alone() {
    let fixture = Fixture::new("task3");
    {
        let index = GenerationStore::open(&fixture.paths.index_db).expect("index");
        index.bootstrap_clock("A").expect("clock");
        let mut index = index;
        let building = index.begin_generation("A").expect("begin");
        index.publish_stable(building.id).expect("publish");
    }
    let work = WorkRuntime::open(
        fixture.workspace,
        &fixture.paths.workspace_db,
        &fixture.paths.index_db,
    )
    .expect("work runtime");
    let item = work
        .create(&NewWorkItem {
            source_kind: WorkItemSourceKind::Issue,
            source_ref: None,
            title: None,
            goal: "g".to_owned(),
        })
        .expect("item")
        .uid;
    work.start(
        item,
        &StartObservation {
            head: None,
            dirty: DirtyObservation::Clean,
            preexisting_dirty: Vec::new(),
            owner_agent: None,
        },
    )
    .expect("start");
    let before = work.snapshot(item, None).expect("snapshot");
    let store = fixture.workspace_store();
    let note = fixture.note_on(
        &store,
        item,
        WorkNoteKind::Proposal,
        SourceKind::AgentReported,
    );
    fixture
        .runtime()
        .promote(&request(
            note,
            PromotionBasis::UserExplicit,
            PromotionTarget::Decision(decision("rustls")),
        ))
        .expect("promote");
    assert_eq!(work.snapshot(item, None).expect("snapshot"), before);
}

// ========================================== migration / schema backstop

#[test]
fn project_v3_migrates_to_v4_with_rows_kept_and_the_receipt_guarded() {
    let dir = TestDir::create("project-v3");
    let path = dir.path().join("project.db");
    {
        let v3 = db::open(
            &path,
            DbKind::Project,
            &schema::project::PROJECT_MIGRATIONS[..3],
        )
        .expect("v3 project.db");
        assert_eq!(v3.schema_version, 3);
        v3.connection
            .execute(
                "INSERT INTO decision (uid, scope_kind, topic, chosen_summary, rationale, status, \
                 source_kind, created_at, updated_at) \
                 VALUES (?1, 'PROJECT', 't', 'c', 'r', 'ACTIVE', 'USER_EXPLICIT', '0', '0')",
                params![DecisionId::generate().to_bytes().to_vec()],
            )
            .expect("legacy decision");
    }
    let opened = schema::project::open(&path).expect("migrates");
    assert_eq!(opened.schema_version, 4);
    let decisions: i64 = opened
        .connection
        .query_row("SELECT COUNT(*) FROM decision", [], |row| row.get(0))
        .expect("count");
    assert_eq!(decisions, 1);

    // The schema refuses receipts the typed gates would refuse.
    let insert =
        |note_kind: &str, target: &str, basis: &str, source: &str, lineage: Option<&str>| {
            opened.connection.execute(
            "INSERT INTO knowledge_promotion (uid, workspace_uid, work_note_uid, work_note_kind, \
             work_note_source_kind, work_note_fingerprint, target_kind, target_uid, \
             request_fingerprint, promotion_basis, authority_source_kind, lineage_kind, \
             lineage_target_uid, created_at) \
             VALUES (randomblob(16), randomblob(16), randomblob(16), ?1, 'AGENT_REPORTED', 'f', \
                     ?2, randomblob(16), 'r', ?3, ?4, ?5, \
                     CASE WHEN ?5 IS NULL THEN NULL ELSE randomblob(16) END, '0')",
            params![note_kind, target, basis, source, lineage],
        )
        };
    assert!(
        insert(
            "PROPOSAL",
            "DECISION",
            "USER_EXPLICIT",
            "USER_EXPLICIT",
            None
        )
        .is_ok()
    );
    for (note_kind, target, basis, source, lineage) in [
        (
            "PROPOSAL",
            "DECISION",
            "USER_EXPLICIT",
            "AGENT_REPORTED",
            None,
        ),
        (
            "OBSERVATION",
            "DECISION",
            "USER_EXPLICIT",
            "USER_EXPLICIT",
            None,
        ),
        (
            "OPEN_QUESTION",
            "POLICY",
            "USER_EXPLICIT",
            "USER_EXPLICIT",
            None,
        ),
        (
            "PROPOSAL",
            "POLICY",
            "VALIDATED_PROJECT_OBSERVATION",
            "OBSERVED",
            None,
        ),
        (
            "PROPOSAL",
            "POLICY",
            "USER_EXPLICIT",
            "USER_EXPLICIT",
            Some("DECISION_REVERSES"),
        ),
    ] {
        assert!(
            insert(note_kind, target, basis, source, lineage).is_err(),
            "{note_kind} {target} {basis} {source} {lineage:?}"
        );
    }
}

// ================================================ boundaries (59-61)

#[test]
fn promotion_source_calls_no_model_command_or_backend() {
    let source = include_str!("../promotion.rs");
    let code = &source[..source.find("#[cfg(test)]").expect("test marker")];
    for forbidden in [
        "Command",
        "std::process",
        "\"git",
        "semantic",
        "lsp",
        "llm",
        "anthropic",
        "reqwest",
        "note_text.split",
        "regex",
    ] {
        assert!(
            !code.contains(forbidden),
            "promotion.rs must not reference {forbidden}"
        );
    }
}

// ================================================= EXPLAIN QUERY PLAN

fn plan(connection: &Connection, sql: &str) -> String {
    let mut statement = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("plan prepares");
    let blob = vec![0_u8; 16];
    let rows: Vec<String> = statement
        .query_map(params![blob, blob], |row| row.get::<_, String>(3))
        .expect("plan runs")
        .collect::<Result<_, _>>()
        .expect("plan decodes");
    rows.join(" | ")
}

/// Task 4 SQL access plan (#20): R1 receipt, T3 state by uid, N1 note by
/// uid. Run with `--nocapture` to print the plans.
#[test]
fn task4_access_paths_use_their_intended_indexes() {
    let fixture = Fixture::new("eqp");
    let project = fixture.project_raw();
    let workspace = Connection::open(&fixture.paths.workspace_db).expect("workspace");
    let cases: [(&str, &Connection, String, &str); 3] = [
        (
            "R1 receipt by workspace + note",
            &project,
            format!(
                "SELECT {RECEIPT_COLUMNS} FROM knowledge_promotion \
                 WHERE workspace_uid = ?1 AND work_note_uid = ?2"
            ),
            "sqlite_autoindex_knowledge_promotion_2 (workspace_uid=? AND work_note_uid=?)",
        ),
        (
            "T3 project state by uid",
            &project,
            "SELECT * FROM project_state WHERE uid = ?1 AND ?2 IS NOT NULL".to_owned(),
            "idx_project_state_uid (uid=?)",
        ),
        (
            "N1 work note by uid",
            &workspace,
            "SELECT * FROM work_note n JOIN work_item w ON w.id = n.work_item_id \
             WHERE n.uid = ?1 AND ?2 IS NOT NULL"
                .to_owned(),
            "sqlite_autoindex_work_note_1 (uid=?)",
        ),
    ];
    for (label, connection, sql, index) in cases {
        let found = plan(connection, &sql);
        println!("{label}: {found}");
        assert!(
            found.contains(index),
            "{label}: expected {index}, got {found}"
        );
        assert!(
            !found.split(" | ").any(|step| step.starts_with("SCAN ")),
            "{label}: unexpected scan in {found}"
        );
    }
}
