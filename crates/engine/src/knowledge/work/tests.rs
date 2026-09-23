//! #20 task 3 acceptance: Workspace-bound WorkItem lifecycle. Case
//! numbers in comments refer to the task 3 acceptance list in #20.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use brainprint_core::{IndexIncarnationId, ResourceId, WorkItemId, WorkspaceId};
use rusqlite::{Connection, params};

use super::*;
use crate::{
    db::{self, DbKind},
    generation::{GenerationError, GenerationStore},
    init::init_workspace,
    knowledge::{
        DirtyState, KnowledgeScope, NewPolicy, NewWorkItem, PriorityClass, ProjectKnowledgeStore,
        ProtectionClass, Provenance, SourceKind, WorkItemSourceKind, workspace::OVERLAP_SQL,
    },
    paths::{GlobalPaths, WorkspacePaths},
    registry::GlobalRegistry,
    schema,
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-work-{label}-{}-{sequence}",
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

/// One initialized (bound) Workspace whose index.db clock/generations the
/// test drives directly -- no scan, no semantic backend.
struct Fixture {
    _home: TestDir,
    root: TestDir,
    global: GlobalPaths,
    id: WorkspaceId,
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
            root,
            global,
            id: outcome.workspace_id,
            paths: WorkspacePaths::from_root(&outcome.workspace_root),
        }
    }

    fn runtime(&self) -> WorkRuntime {
        WorkRuntime::open(self.id, &self.paths.workspace_db, &self.paths.index_db)
            .expect("bound runtime opens")
    }

    fn index(&self) -> GenerationStore {
        GenerationStore::open(&self.paths.index_db).expect("index.db")
    }

    fn incarnation(&self) -> IndexIncarnationId {
        self.index().index_incarnation_id().expect("incarnation")
    }

    /// Move the Workspace clock to `revision` (bootstrapping it if needed).
    fn advance(&self, revision: &str) {
        let index = self.index();
        index.bootstrap_clock(revision).expect("bootstrap");
        index
            .set_current_workspace_revision(revision)
            .expect("advance");
    }

    /// Advance to `revision` and publish a STABLE generation for it.
    fn publish(&self, revision: &str) -> i64 {
        self.advance(revision);
        let mut index = self.index();
        let building = index.begin_generation(revision).expect("begin");
        index
            .publish_stable(building.id)
            .expect("publish")
            .generation_no
    }

    /// Delete index.db (and WAL sidecars) and let repeated init recreate
    /// it, as the Task 1 rebuild acceptance does.
    fn rebuild_index(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", self.paths.index_db.display()));
        }
        init_workspace(self.root.path(), &self.global).expect("re-init");
    }
}

fn item(goal: &str) -> NewWorkItem {
    NewWorkItem {
        source_kind: WorkItemSourceKind::Issue,
        source_ref: Some("#20".to_owned()),
        title: None,
        goal: goal.to_owned(),
    }
}

fn start_unknown() -> StartObservation {
    StartObservation {
        head: Some("44824bc".to_owned()),
        dirty: DirtyObservation::Unknown,
        preexisting_dirty: Vec::new(),
        owner_agent: Some("agent-a".to_owned()),
    }
}

fn dirty(fingerprint: &str) -> DirtyObservation {
    DirtyObservation::Dirty {
        fingerprint: fingerprint.to_owned(),
    }
}

fn result(summary: &str, commit: Option<&str>, remaining: DirtyObservation) -> ResultObservation {
    ResultObservation {
        summary: summary.to_owned(),
        commit_id: commit.map(str::to_owned),
        change_set_fingerprint: None,
        verification_summary: None,
        remaining_dirty: remaining,
    }
}

fn evidence(resource: ResourceId, role: WorkResourceRole) -> ResourceEvidence {
    ResourceEvidence {
        resource,
        role,
        locator_hint: None,
    }
}

fn handoff(work_item: WorkItemId, summary: &str) -> WorkHandoff {
    WorkHandoff {
        work_item,
        handoff_summary: summary.to_owned(),
        remaining_summary: None,
        blocker_summary: None,
        next_scope_hint: None,
        created_at: String::new(),
    }
}

fn started(runtime: &WorkRuntime, goal: &str) -> WorkItemId {
    let created = runtime.create(&item(goal)).expect("create");
    runtime.start(created.uid, &start_unknown()).expect("start");
    created.uid
}

fn is_transition_error(error: &WorkError) -> bool {
    matches!(
        error,
        WorkError::Knowledge(KnowledgeError::InvalidTransition { .. })
    )
}

fn baseline(
    runtime: &WorkRuntime,
    work_item: WorkItemId,
) -> (String, i64, Option<String>, DirtyObservation) {
    let state = runtime
        .snapshot(work_item, None)
        .expect("snapshot")
        .working_state
        .expect("started");
    (
        state.baseline_workspace_revision,
        state.baseline_generation_no,
        state.baseline_head,
        state.baseline_dirty,
    )
}

// ================================================ binding (cases 1-3)

#[test]
fn runtime_rejects_unbound_workspace_db() {
    // 1.
    let fixture = Fixture::new("unbound-ws");
    let loose = TestDir::create("unbound-ws-file");
    let unbound = loose.path().join("workspace.db");
    drop(WorkspaceKnowledgeStore::open(&unbound).expect("plain store opens unbound"));
    assert!(matches!(
        WorkRuntime::open(fixture.id, &unbound, &fixture.paths.index_db),
        Err(WorkError::UnboundWorkspace { db: "workspace.db" })
    ));
    // Nothing was bound as a side effect.
    let store = WorkspaceKnowledgeStore::open(&unbound).expect("reopen");
    assert_eq!(store.bound_workspace_id().expect("read"), None);
    // A missing file is reported, not created.
    let missing = loose.path().join("missing.db");
    assert!(matches!(
        WorkRuntime::open(fixture.id, &missing, &fixture.paths.index_db),
        Err(WorkError::MissingDatabase { db: "workspace.db" })
    ));
    assert!(!missing.exists());
}

#[test]
fn runtime_rejects_unbound_index_db() {
    let fixture = Fixture::new("unbound-index");
    let loose = TestDir::create("unbound-index-file");
    let unbound = loose.path().join("index.db");
    drop(GenerationStore::open(&unbound).expect("plain index opens unbound"));
    assert!(matches!(
        WorkRuntime::open(fixture.id, &fixture.paths.workspace_db, &unbound),
        Err(WorkError::UnboundWorkspace { db: "index.db" })
    ));
}

#[test]
fn runtime_rejects_wrong_workspace_ids() {
    // 2, 3: wrong expected WorkspaceID, and two DBs of different Workspaces.
    let a = Fixture::new("mismatch-a");
    let b = Fixture::new("mismatch-b");
    assert!(matches!(
        WorkRuntime::open(
            WorkspaceId::generate(),
            &a.paths.workspace_db,
            &a.paths.index_db
        ),
        Err(WorkError::WorkspaceMismatch {
            db: "workspace.db",
            ..
        })
    ));
    assert!(matches!(
        WorkRuntime::open(a.id, &a.paths.workspace_db, &b.paths.index_db),
        Err(WorkError::WorkspaceMismatch { db: "index.db", found, .. }) if found == b.id
    ));
    assert!(matches!(
        WorkRuntime::open(b.id, &a.paths.workspace_db, &b.paths.index_db),
        Err(WorkError::WorkspaceMismatch { db: "workspace.db", found, .. }) if found == a.id
    ));
}

// ============================================ activation (cases 4-8)

#[test]
fn several_work_items_are_active_at_once_and_none_is_current() {
    // 4, 33, 34.
    let fixture = Fixture::new("multi-active");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let first = started(&runtime, "parser");
    let second = started(&runtime, "projection");
    let third = started(&runtime, "docs");
    for uid in [first, second, third] {
        let snapshot = runtime.snapshot(uid, None).expect("snapshot");
        assert_eq!(snapshot.item.uid, uid, "the snapshot is of the named item");
        assert_eq!(snapshot.item.status, WorkItemStatus::Active);
        assert_eq!(snapshot.workspace_id, fixture.id);
    }
    // An unknown id is NotFound, never some other ACTIVE item.
    assert!(matches!(
        runtime.snapshot(WorkItemId::generate(), None),
        Err(WorkError::Knowledge(KnowledgeError::NotFound { .. }))
    ));
}

#[test]
fn first_activation_needs_a_current_stable_generation() {
    // 5, 6, 7.
    let fixture = Fixture::new("not-ready");
    let runtime = fixture.runtime();
    let uid = runtime.create(&item("g")).expect("create").uid;

    assert!(matches!(
        runtime.start(uid, &start_unknown()),
        Err(WorkError::WorkspaceNotReady(NotReady::ClockNotBootstrapped))
    ));
    fixture.advance("A");
    assert!(matches!(
        runtime.start(uid, &start_unknown()),
        Err(WorkError::WorkspaceNotReady(NotReady::NoStableGeneration))
    ));
    fixture.publish("A");
    fixture.advance("B");
    assert!(matches!(
        runtime.start(uid, &start_unknown()),
        Err(WorkError::WorkspaceNotReady(NotReady::StableBehind { stable_basis, current }))
            if stable_basis == "A" && current == "B"
    ));
    // No ACTIVE status and no baseline were written by the failures.
    let snapshot = runtime.snapshot(uid, None).expect("snapshot");
    assert_eq!(snapshot.item.status, WorkItemStatus::Open);
    assert_eq!(snapshot.working_state, None);
}

#[test]
fn first_activation_stores_the_exact_observed_baseline() {
    // 8.
    let fixture = Fixture::new("exact-baseline");
    fixture.publish("A");
    let generation = fixture.publish("B");
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    let snapshot = runtime.snapshot(uid, None).expect("snapshot");
    let state = snapshot.working_state.expect("baseline");
    assert_eq!(state.baseline_workspace_revision, "B");
    assert_eq!(state.baseline_generation_no, generation);
    assert_eq!(state.baseline_head.as_deref(), Some("44824bc"));
    assert_eq!(state.last_observed_workspace_revision, "B");
    assert_eq!(state.owner_agent.as_deref(), Some("agent-a"));
    assert_eq!(
        snapshot.baseline_generation,
        Some(GenerationReference {
            index_incarnation: Some(fixture.incarnation()),
            generation_no: generation,
            workspace_revision: "B".to_owned(),
            state: GenerationReferenceState::PresentMatching,
        })
    );
    // A second start cannot re-fix the baseline.
    fixture.publish("C");
    assert!(is_transition_error(
        &runtime
            .start(uid, &start_unknown())
            .expect_err("already started")
    ));
    assert_eq!(baseline(&runtime, uid).0, "B");
}

// ========================================= immutability (cases 9-11)

#[test]
fn baseline_survives_progress_block_pause_and_resume() {
    // 9, 10, 11, and #20 task 3 case 37.
    let fixture = Fixture::new("immutable");
    let generation = fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    let fixed = baseline(&runtime, uid);
    assert_eq!((fixed.0.as_str(), fixed.1), ("A", generation));

    fixture.publish("B");
    let progressed = runtime
        .update_progress(
            uid,
            &WorkProgress {
                current_step: Some("parser".to_owned()),
                progress_summary: Some("lexer done".to_owned()),
                remaining_summary: Some("grammar".to_owned()),
                blocker_summary: None,
                owner_agent: Some("agent-b".to_owned()),
            },
            &[],
        )
        .expect("progress");
    assert_eq!(progressed.last_observed_workspace_revision, "B");
    assert_eq!(progressed.owner_agent.as_deref(), Some("agent-b"));
    assert_eq!(baseline(&runtime, uid), fixed);

    let blocked = runtime.block(uid, "waiting for review").expect("block");
    assert_eq!(blocked.status, WorkItemStatus::Blocked);
    let state = runtime
        .snapshot(uid, None)
        .expect("s")
        .working_state
        .expect("state");
    assert_eq!(state.blocker_summary.as_deref(), Some("waiting for review"));
    assert_eq!(baseline(&runtime, uid), fixed);
    assert_eq!(
        runtime.resume(uid).expect("resume").status,
        WorkItemStatus::Active
    );
    assert_eq!(baseline(&runtime, uid), fixed);

    fixture.advance("C");
    let paused = runtime.pause(uid).expect("pause");
    assert_eq!(paused.status, WorkItemStatus::Paused);
    let state = runtime
        .snapshot(uid, None)
        .expect("s")
        .working_state
        .expect("state");
    assert_eq!(state.last_observed_workspace_revision, "C");
    assert_eq!(baseline(&runtime, uid), fixed);
    // A pause invents no blocker beyond the one already recorded.
    assert_eq!(state.blocker_summary.as_deref(), Some("waiting for review"));
    assert!(is_transition_error(
        &runtime
            .start(uid, &start_unknown())
            .expect_err("start is not resume")
    ));
    assert_eq!(
        runtime.resume(uid).expect("resume").status,
        WorkItemStatus::Active
    );
    runtime
        .update_progress(uid, &WorkProgress::default(), &[])
        .expect("progress again");
    assert_eq!(baseline(&runtime, uid), fixed);

    // Only BLOCKED / PAUSED resume; only ACTIVE blocks or pauses.
    assert!(is_transition_error(
        &runtime.resume(uid).expect_err("already active")
    ));
    runtime.pause(uid).expect("pause");
    assert!(is_transition_error(
        &runtime.block(uid, "x").expect_err("paused")
    ));
    assert!(is_transition_error(
        &runtime.pause(uid).expect_err("paused")
    ));
}

// ================================== dirty observation (cases 12-17)

#[test]
fn baseline_dirty_states_round_trip() {
    // 12, 13, 14.
    let fixture = Fixture::new("dirty-baseline");
    fixture.publish("A");
    let runtime = fixture.runtime();
    for observation in [
        DirtyObservation::Unknown,
        DirtyObservation::Clean,
        dirty("fp-1"),
    ] {
        let uid = runtime.create(&item("g")).expect("create").uid;
        runtime
            .start(
                uid,
                &StartObservation {
                    dirty: observation.clone(),
                    ..start_unknown()
                },
            )
            .expect("start");
        let stored = baseline(&runtime, uid).3;
        assert_eq!(stored, observation);
        assert_eq!(
            stored.fingerprint().is_some(),
            stored.state() == DirtyState::Dirty
        );
    }
}

#[test]
fn malformed_dirty_observations_are_rejected() {
    // 14, 15.
    for (state, fingerprint) in [
        (DirtyState::Unknown, Some("fp")),
        (DirtyState::Clean, Some("fp")),
        (DirtyState::Dirty, None),
        (DirtyState::Dirty, Some("")),
    ] {
        assert!(
            DirtyObservation::from_parts(state, fingerprint.map(str::to_owned)).is_err(),
            "{state} + {fingerprint:?}"
        );
    }
    let fixture = Fixture::new("dirty-invalid");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = runtime.create(&item("g")).expect("create").uid;
    assert!(
        runtime
            .start(
                uid,
                &StartObservation {
                    dirty: dirty(""),
                    ..start_unknown()
                }
            )
            .is_err()
    );
    // Pre-existing dirty Resources contradict a non-DIRTY observation.
    for observation in [DirtyObservation::Unknown, DirtyObservation::Clean] {
        assert!(matches!(
            runtime.start(
                uid,
                &StartObservation {
                    dirty: observation,
                    preexisting_dirty: vec![ResourceObservation {
                        resource: ResourceId::generate(),
                        locator_hint: None,
                    }],
                    ..start_unknown()
                }
            ),
            Err(WorkError::InvalidObservation(_))
        ));
    }
    assert_eq!(
        runtime.snapshot(uid, None).expect("s").item.status,
        WorkItemStatus::Open
    );

    // The schema CHECK refuses the same combinations at the storage layer.
    let connection = Connection::open(&fixture.paths.workspace_db).expect("raw");
    let item_id: i64 = connection
        .query_row("SELECT id FROM work_item", [], |row| row.get(0))
        .expect("item row");
    for (state, fingerprint) in [
        ("UNKNOWN", Some("fp")),
        ("CLEAN", Some("fp")),
        ("DIRTY", None),
        ("MAYBE", None),
    ] {
        let inserted = connection.execute(
            "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
             baseline_generation_no, baseline_dirty_state, baseline_dirty_fingerprint, \
             last_observed_workspace_revision, updated_at) VALUES (?1, 'A', 1, ?2, ?3, 'A', '0')",
            params![item_id, state, fingerprint],
        );
        assert!(
            inserted.is_err(),
            "{state} + {fingerprint:?} must violate CHECK"
        );
    }
}

#[test]
fn legacy_rows_migrate_to_conservative_dirty_states() {
    // 16, 17, and legacy Work Result → UNKNOWN (case 27 side).
    let dir = TestDir::create("legacy");
    let path = dir.path().join("workspace.db");
    let (fingerprinted, unfingerprinted) = (WorkItemId::generate(), WorkItemId::generate());
    {
        let v3 = db::open(
            &path,
            DbKind::Workspace,
            &schema::workspace::WORKSPACE_MIGRATIONS[..3],
        )
        .expect("v3 workspace.db");
        assert_eq!(v3.schema_version, 3);
        for (uid, fingerprint) in [(fingerprinted, Some("legacy-fp")), (unfingerprinted, None)] {
            v3.connection
                .execute(
                    "INSERT INTO work_item (uid, source_kind, goal, status, created_at) \
                     VALUES (?1, 'ISSUE', 'g', 'ACTIVE', '0')",
                    params![uid.to_bytes().to_vec()],
                )
                .expect("item");
            v3.connection
                .execute(
                    "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
                     baseline_generation_no, baseline_dirty_fingerprint, \
                     last_observed_workspace_revision, updated_at) \
                     SELECT id, 'r0', 1, ?2, 'r0', '0' FROM work_item WHERE uid = ?1",
                    params![uid.to_bytes().to_vec(), fingerprint],
                )
                .expect("state");
        }
        v3.connection
            .execute(
                "INSERT INTO work_result (work_item_id, result_status, result_summary, \
                 result_workspace_revision, created_at) \
                 SELECT id, 'PARTIAL', 'legacy', 'r0', '0' FROM work_item WHERE uid = ?1",
                params![unfingerprinted.to_bytes().to_vec()],
            )
            .expect("result");
    }

    let store = WorkspaceKnowledgeStore::open(&path).expect("migrates to current");
    let state = |uid| store.get_working_state(uid).expect("get").expect("state");
    assert_eq!(state(fingerprinted).baseline_dirty, dirty("legacy-fp"));
    assert_eq!(
        state(unfingerprinted).baseline_dirty,
        DirtyObservation::Unknown
    );
    assert_eq!(
        store
            .get_work_result(unfingerprinted)
            .expect("get")
            .expect("result")
            .remaining_dirty,
        DirtyObservation::Unknown
    );
    drop(store);
    // The parked-fingerprint temp table did not leak into the schema.
    let raw = Connection::open(&path).expect("raw");
    let version: u32 = raw
        .query_row(
            "SELECT schema_version FROM db_meta WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .expect("version");
    assert_eq!(version, 5);
}

// =============================== pre-existing dirty (cases 18-20)

#[test]
fn preexisting_dirty_and_touched_roles_coexist_without_claiming_current_dirt() {
    // 18, 19, 20.
    let fixture = Fixture::new("preexisting");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let (logs, source) = (ResourceId::generate(), ResourceId::generate());
    let uid = runtime.create(&item("g")).expect("create").uid;
    runtime
        .start(
            uid,
            &StartObservation {
                dirty: dirty("baseline-fp"),
                preexisting_dirty: vec![ResourceObservation {
                    resource: logs,
                    locator_hint: Some(".dev-logs/run.log".to_owned()),
                }],
                ..start_unknown()
            },
        )
        .expect("start");
    fixture.advance("B");
    runtime
        .update_progress(
            uid,
            &WorkProgress::default(),
            &[
                evidence(logs, WorkResourceRole::Touched),
                evidence(source, WorkResourceRole::Target),
            ],
        )
        .expect("progress");
    // PREEXISTING_DIRTY is baseline-only.
    assert!(matches!(
        runtime.update_progress(
            uid,
            &WorkProgress::default(),
            &[evidence(source, WorkResourceRole::PreexistingDirty)]
        ),
        Err(WorkError::InvalidObservation(_))
    ));

    let roles_of = |snapshot: &WorkSnapshot, resource| {
        snapshot
            .resources
            .iter()
            .filter(|row| row.resource == resource)
            .map(|row| row.role)
            .collect::<Vec<_>>()
    };
    let snapshot = runtime.snapshot(uid, None).expect("snapshot");
    assert_eq!(
        roles_of(&snapshot, logs),
        vec![
            WorkResourceRole::PreexistingDirty,
            WorkResourceRole::Touched
        ]
    );
    assert_eq!(roles_of(&snapshot, source), vec![WorkResourceRole::Target]);
    let preexisting = &snapshot.resources[0];
    assert_eq!(preexisting.first_observed_revision, "A");
    assert_eq!(
        preexisting.locator_hint.as_deref(),
        Some(".dev-logs/run.log")
    );

    // The roles stay as evidence; current dirt is the result's observation.
    let done = runtime
        .complete(
            uid,
            &result("done", Some("c0ffee"), DirtyObservation::Clean),
            None,
        )
        .expect("complete");
    assert_eq!(roles_of(&done, logs).len(), 2);
    assert_eq!(
        done.result.expect("result").remaining_dirty,
        DirtyObservation::Clean
    );
}

// ======================================= results (cases 21-31)

#[test]
fn a_committed_partial_result_does_not_complete_the_item() {
    // 21, 22, 23, 24.
    let fixture = Fixture::new("partial");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    let partial = runtime
        .record_partial(
            uid,
            &ResultObservation {
                remaining_dirty: dirty("wip"),
                ..result(
                    "parser changes committed",
                    Some("abc123"),
                    DirtyObservation::Unknown,
                )
            },
        )
        .expect("partial");
    assert_eq!(partial.result_status, WorkResultStatus::Partial);
    assert_eq!(partial.commit_id.as_deref(), Some("abc123"));
    let snapshot = runtime.snapshot(uid, None).expect("snapshot");
    assert_eq!(snapshot.item.status, WorkItemStatus::Active);
    assert_eq!(snapshot.item.closed_at, None);

    // A partial on a PAUSED item keeps it PAUSED.
    runtime.pause(uid).expect("pause");
    runtime
        .record_partial(
            uid,
            &result("checkpoint", Some("abc124"), DirtyObservation::Clean),
        )
        .expect("partial while paused");
    assert_eq!(
        runtime.snapshot(uid, None).expect("s").item.status,
        WorkItemStatus::Paused
    );
    runtime.resume(uid).expect("resume");

    // Explicit completion needs no commit.
    let done = runtime
        .complete(
            uid,
            &result("no-op change", None, DirtyObservation::Clean),
            None,
        )
        .expect("complete");
    assert_eq!(done.item.status, WorkItemStatus::Completed);
    let final_result = done.result.expect("result");
    assert_eq!(final_result.result_status, WorkResultStatus::Completed);
    assert_eq!(final_result.commit_id, None);
}

#[test]
fn final_results_keep_all_three_dirty_observations() {
    // 25, 26, 27.
    let fixture = Fixture::new("final-dirty");
    fixture.publish("A");
    let runtime = fixture.runtime();
    for remaining in [
        DirtyObservation::Clean,
        dirty("left-over"),
        DirtyObservation::Unknown,
    ] {
        let uid = started(&runtime, "g");
        let done = runtime
            .complete(uid, &result("done", None, remaining.clone()), None)
            .expect("complete");
        assert_eq!(done.result.expect("result").remaining_dirty, remaining);
    }
}

#[test]
fn completion_writes_result_status_snapshot_and_handoff_together() {
    // 28, 39.
    let fixture = Fixture::new("complete");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    let generation = fixture.publish("B");
    let done = runtime
        .complete(
            uid,
            &ResultObservation {
                summary: "shipped".to_owned(),
                commit_id: Some("feed".to_owned()),
                change_set_fingerprint: Some("cs-1".to_owned()),
                verification_summary: Some("cargo test ok".to_owned()),
                remaining_dirty: DirtyObservation::Clean,
            },
            Some(&WorkHandoff {
                remaining_summary: Some("docs".to_owned()),
                next_scope_hint: Some("crates/engine/src/knowledge".to_owned()),
                ..handoff(uid, "task 3 done")
            }),
        )
        .expect("complete");
    assert_eq!(done.item.status, WorkItemStatus::Completed);
    assert!(done.item.closed_at.is_some());
    let stored = done.result.expect("result");
    assert_eq!(stored.result_status, WorkResultStatus::Completed);
    assert_eq!(stored.commit_id.as_deref(), Some("feed"));
    assert_eq!(stored.change_set_fingerprint.as_deref(), Some("cs-1"));
    assert_eq!(
        stored.verification_summary.as_deref(),
        Some("cargo test ok")
    );
    assert_eq!(stored.result_workspace_revision, "B");
    assert_eq!(stored.result_generation_no, Some(generation));
    assert_eq!(
        done.result_generation.expect("reference").state,
        GenerationReferenceState::PresentMatching
    );
    assert_eq!(
        done.working_state
            .expect("state")
            .last_observed_workspace_revision,
        "B"
    );
    assert_eq!(
        done.latest_handoff.expect("handoff").handoff_summary,
        "task 3 done"
    );
    assert_eq!(done.staleness, Staleness::Closed);
}

#[test]
fn a_rejected_finalization_writes_nothing() {
    // Finalization atomicity: complete is only valid from ACTIVE; a failed
    // transition leaves no COMPLETED result and no handoff behind.
    let fixture = Fixture::new("atomic");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    runtime.pause(uid).expect("pause");
    let error = runtime
        .complete(
            uid,
            &result("done", Some("x"), DirtyObservation::Clean),
            Some(&handoff(uid, "h")),
        )
        .expect_err("PAUSED cannot complete");
    assert!(is_transition_error(&error));
    let snapshot = runtime.snapshot(uid, None).expect("snapshot");
    assert_eq!(snapshot.item.status, WorkItemStatus::Paused);
    assert_eq!(snapshot.result, None);
    assert_eq!(snapshot.latest_handoff, None);
    // A handoff naming another WorkItem is refused before any write.
    let other = started(&runtime, "other");
    assert!(matches!(
        runtime.complete(
            other,
            &result("d", None, DirtyObservation::Clean),
            Some(&handoff(uid, "h"))
        ),
        Err(WorkError::InvalidObservation(_))
    ));
    assert_eq!(
        runtime.snapshot(other, None).expect("s").item.status,
        WorkItemStatus::Active
    );
}

#[test]
fn terminal_items_reject_every_further_mutation() {
    // 29, 30, 31.
    let fixture = Fixture::new("terminal");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let completed = started(&runtime, "c");
    runtime
        .complete(
            completed,
            &result("done", None, DirtyObservation::Clean),
            None,
        )
        .expect("complete");
    let abandoned = started(&runtime, "a");
    runtime.pause(abandoned).expect("pause");
    let gone = runtime
        .abandon(
            abandoned,
            &result("dropped", None, DirtyObservation::Unknown),
            None,
        )
        .expect("abandon from PAUSED");
    assert_eq!(gone.item.status, WorkItemStatus::Abandoned);
    assert!(gone.item.closed_at.is_some());
    assert_eq!(
        gone.result.expect("result").result_status,
        WorkResultStatus::Abandoned
    );
    // An OPEN item may be abandoned without ever having a baseline.
    let never = runtime.create(&item("never")).expect("create").uid;
    let never = runtime
        .abandon(
            never,
            &result("not needed", None, DirtyObservation::Unknown),
            None,
        )
        .expect("abandon OPEN");
    assert_eq!(never.working_state, None);

    for uid in [completed, abandoned] {
        let before = runtime.snapshot(uid, None).expect("snapshot");
        assert!(is_transition_error(
            &runtime.resume(uid).expect_err("resume")
        ));
        assert!(is_transition_error(&runtime.pause(uid).expect_err("pause")));
        assert!(is_transition_error(
            &runtime.block(uid, "b").expect_err("block")
        ));
        assert!(is_transition_error(
            &runtime.start(uid, &start_unknown()).expect_err("reopen")
        ));
        assert!(is_transition_error(
            &runtime
                .update_progress(uid, &WorkProgress::default(), &[])
                .expect_err("progress")
        ));
        assert!(is_transition_error(
            &runtime
                .record_partial(uid, &result("p", None, DirtyObservation::Clean))
                .expect_err("partial")
        ));
        assert!(is_transition_error(
            &runtime
                .complete(uid, &result("again", None, DirtyObservation::Clean), None)
                .expect_err("second completion")
        ));
        assert!(is_transition_error(
            &runtime
                .abandon(uid, &result("again", None, DirtyObservation::Clean), None)
                .expect_err("re-abandon")
        ));
        assert!(is_transition_error(
            &runtime
                .add_handoff(&handoff(uid, "late"))
                .expect_err("handoff")
        ));
        assert_eq!(runtime.snapshot(uid, None).expect("snapshot"), before);
    }
}

// ====================================== handoff / stale (32, 35-37)

#[test]
fn latest_handoff_is_one_bounded_row() {
    // 32.
    let fixture = Fixture::new("handoff");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    assert_eq!(runtime.snapshot(uid, None).expect("s").latest_handoff, None);
    for summary in ["first", "second", "third"] {
        runtime
            .add_handoff(&handoff(uid, summary))
            .expect("handoff");
    }
    assert_eq!(
        runtime
            .snapshot(uid, None)
            .expect("s")
            .latest_handoff
            .expect("latest")
            .handoff_summary,
        "third"
    );
}

#[test]
fn staleness_needs_an_explicit_cutoff_and_never_changes_status() {
    // 35, 36, 37.
    let fixture = Fixture::new("stale");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    let open = runtime.create(&item("open")).expect("create").uid;
    assert_eq!(
        runtime.snapshot(uid, None).expect("s").staleness,
        Staleness::NotEvaluated
    );
    assert_eq!(
        runtime.snapshot(uid, Some(0)).expect("s").staleness,
        Staleness::CurrentAtCutoff
    );
    for target in [uid, open] {
        let snapshot = runtime.snapshot(target, Some(u64::MAX)).expect("s");
        assert_eq!(snapshot.staleness, Staleness::PossiblyStale);
    }
    assert_eq!(
        runtime.snapshot(uid, None).expect("s").item.status,
        WorkItemStatus::Active,
        "stale evaluation never abandons"
    );
    assert_eq!(
        runtime.snapshot(open, None).expect("s").item.status,
        WorkItemStatus::Open
    );
}

// ============================================= overlap (38-43)

#[test]
fn overlap_reports_edit_scope_intersections_only() {
    // 38, 39, 40, 41, 42, 43.
    let fixture = Fixture::new("overlap");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let (shared, related, dirty_only, mine, theirs) = (
        ResourceId::generate(),
        ResourceId::generate(),
        ResourceId::generate(),
        ResourceId::generate(),
        ResourceId::generate(),
    );
    let this = started(&runtime, "this");
    let other = started(&runtime, "other");
    let disjoint = started(&runtime, "disjoint");
    runtime
        .update_progress(
            this,
            &WorkProgress::default(),
            &[
                evidence(shared, WorkResourceRole::Target),
                evidence(shared, WorkResourceRole::Touched),
                evidence(related, WorkResourceRole::Target),
                evidence(dirty_only, WorkResourceRole::Touched),
                evidence(mine, WorkResourceRole::Owned),
            ],
        )
        .expect("this");
    runtime
        .update_progress(
            disjoint,
            &WorkProgress::default(),
            &[evidence(theirs, WorkResourceRole::Target)],
        )
        .expect("disjoint");
    assert_eq!(runtime.overlaps(disjoint).expect("overlap"), vec![]);

    // RELATED-only on the other side is not an edit overlap.
    runtime
        .update_progress(
            other,
            &WorkProgress::default(),
            &[evidence(related, WorkResourceRole::Related)],
        )
        .expect("other");
    assert_eq!(runtime.overlaps(this).expect("overlap"), vec![]);

    // PREEXISTING_DIRTY-only on the other side is not either.
    let preexisting = runtime.create(&item("dirty")).expect("create").uid;
    runtime
        .start(
            preexisting,
            &StartObservation {
                dirty: dirty("fp"),
                preexisting_dirty: vec![ResourceObservation {
                    resource: dirty_only,
                    locator_hint: None,
                }],
                ..start_unknown()
            },
        )
        .expect("start");
    assert_eq!(runtime.overlaps(this).expect("overlap"), vec![]);

    // TOUCHED / OWNED on the other side are.
    runtime
        .update_progress(
            other,
            &WorkProgress::default(),
            &[
                evidence(shared, WorkResourceRole::Touched),
                evidence(shared, WorkResourceRole::Owned),
                evidence(mine, WorkResourceRole::Target),
            ],
        )
        .expect("other");
    runtime.block(other, "review").expect("block");
    let overlaps = runtime.overlaps(this).expect("overlap");
    let mut expected = vec![
        WorkOverlap {
            other,
            other_status: WorkItemStatus::Blocked,
            resource: shared,
            this_roles: vec![WorkResourceRole::Target, WorkResourceRole::Touched],
            other_roles: vec![WorkResourceRole::Owned, WorkResourceRole::Touched],
        },
        WorkOverlap {
            other,
            other_status: WorkItemStatus::Blocked,
            resource: mine,
            this_roles: vec![WorkResourceRole::Owned],
            other_roles: vec![WorkResourceRole::Target],
        },
    ];
    expected.sort_by_key(|overlap| overlap.resource.to_bytes());
    assert_eq!(overlaps, expected);
    assert!(
        overlaps.iter().all(|overlap| overlap.other != this),
        "self excluded"
    );

    // The signal blocks nothing: both sides keep working.
    runtime
        .update_progress(
            this,
            &WorkProgress::default(),
            &[evidence(shared, WorkResourceRole::Touched)],
        )
        .expect("this proceeds");
    runtime.resume(other).expect("other proceeds");
    assert_eq!(
        runtime.snapshot(this, None).expect("s").item.status,
        WorkItemStatus::Active
    );
    assert_eq!(
        runtime.snapshot(other, None).expect("s").item.status,
        WorkItemStatus::Active
    );

    // Terminal items drop out of the signal.
    runtime
        .complete(other, &result("done", None, DirtyObservation::Clean), None)
        .expect("complete");
    assert_eq!(runtime.overlaps(this).expect("overlap"), vec![]);
}

// =================================== isolation / rebuild (44-48)

fn run_git(args: &[&str], cwd: &Path) {
    let output = process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "brainprint-test")
        .env("GIT_AUTHOR_EMAIL", "test@brainprint.invalid")
        .env("GIT_COMMITTER_NAME", "brainprint-test")
        .env("GIT_COMMITTER_EMAIL", "test@brainprint.invalid")
        .output()
        .expect("git should be runnable");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn secondary_worktree_shares_project_knowledge_but_not_work_items() {
    // 44 (real `git worktree`).
    let home = TestDir::create("wt-home");
    let main = TestDir::create("wt-main");
    let secondary = TestDir::create("wt-secondary");
    fs::remove_dir_all(secondary.path()).expect("worktree target must not exist yet");
    run_git(&["init", "-q"], main.path());
    run_git(
        &["commit", "-q", "--allow-empty", "-m", "init"],
        main.path(),
    );
    run_git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            &secondary.path().to_string_lossy(),
        ],
        main.path(),
    );
    let global = GlobalPaths::from_home(home.path());
    let main_init = init_workspace(main.path(), &global).expect("main init");
    let secondary_init = init_workspace(secondary.path(), &global).expect("secondary init");
    assert_eq!(main_init.project_id, secondary_init.project_id);
    assert_ne!(main_init.workspace_id, secondary_init.workspace_id);
    let main_paths = WorkspacePaths::from_root(&main_init.workspace_root);
    let secondary_paths = WorkspacePaths::from_root(&secondary_init.workspace_root);

    // Project knowledge: one project-home project.db for both.
    let registry = GlobalRegistry::open(&global.global_db).expect("registry");
    let policy = ProjectKnowledgeStore::open_project_home(&registry, main_init.project_id)
        .expect("main project")
        .insert_policy(&NewPolicy {
            scope: KnowledgeScope::project(),
            policy_key: None,
            title: "shared".to_owned(),
            rule_text: "shared rule".to_owned(),
            structured_rule: None,
            protection_class: ProtectionClass::Normal,
            priority_class: PriorityClass::Default,
            provenance: Provenance::new(SourceKind::UserExplicit),
        })
        .expect("policy");
    assert!(
        ProjectKnowledgeStore::open_project_home(&registry, secondary_init.project_id)
            .expect("secondary project")
            .get_policy(policy.uid)
            .expect("get")
            .is_some()
    );

    // Working State: per Workspace.
    for (paths, revision) in [(&main_paths, "M"), (&secondary_paths, "S")] {
        let index = GenerationStore::open(&paths.index_db).expect("index");
        index.bootstrap_clock(revision).expect("clock");
        let mut index = index;
        let building = index.begin_generation(revision).expect("begin");
        index.publish_stable(building.id).expect("publish");
    }
    let main_runtime = WorkRuntime::open(
        main_init.workspace_id,
        &main_paths.workspace_db,
        &main_paths.index_db,
    )
    .expect("main runtime");
    let secondary_runtime = WorkRuntime::open(
        secondary_init.workspace_id,
        &secondary_paths.workspace_db,
        &secondary_paths.index_db,
    )
    .expect("secondary runtime");
    // Same source path in both Workspaces: two independent WorkItems.
    let a = started(&main_runtime, "same issue, main");
    let b = started(&secondary_runtime, "same issue, feature");
    assert_ne!(a, b);
    assert!(matches!(
        secondary_runtime.snapshot(a, None),
        Err(WorkError::Knowledge(KnowledgeError::NotFound { .. }))
    ));
    assert!(matches!(
        main_runtime.snapshot(b, None),
        Err(WorkError::Knowledge(KnowledgeError::NotFound { .. }))
    ));
    assert!(secondary_runtime.pause(a).is_err());
    // B cannot open A's workspace.db (or A's index.db) as B.
    assert!(matches!(
        WorkRuntime::open(
            secondary_init.workspace_id,
            &main_paths.workspace_db,
            &secondary_paths.index_db
        ),
        Err(WorkError::WorkspaceMismatch {
            db: "workspace.db",
            ..
        })
    ));
    assert!(matches!(
        WorkRuntime::open(
            secondary_init.workspace_id,
            &secondary_paths.workspace_db,
            &main_paths.index_db
        ),
        Err(WorkError::WorkspaceMismatch { db: "index.db", .. })
    ));
    assert_eq!(
        main_runtime
            .snapshot(a, None)
            .expect("a")
            .working_state
            .expect("s")
            .baseline_workspace_revision,
        "M"
    );
}

#[test]
fn generation_references_detect_missing_and_reused_numbers() {
    // 45, 46, 47, 48 (mandatory rebuild / number-reuse acceptance).
    let fixture = Fixture::new("reuse");
    let old_generation = fixture.publish("A");
    assert_eq!(old_generation, 1);
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    runtime
        .record_partial(
            uid,
            &result("checkpoint", Some("abc"), DirtyObservation::Clean),
        )
        .expect("partial");
    let before = runtime.snapshot(uid, None).expect("snapshot");
    let old_incarnation = fixture.incarnation();
    let present = GenerationReference {
        index_incarnation: Some(old_incarnation),
        generation_no: 1,
        workspace_revision: "A".to_owned(),
        state: GenerationReferenceState::PresentMatching,
    };
    assert_eq!(before.baseline_generation, Some(present.clone()));
    assert_eq!(before.result_generation, Some(present));
    drop(runtime);

    // Rebuild: index.db is recreated with no generations at all.
    fixture.rebuild_index();
    let runtime = fixture.runtime();
    let historical = GenerationReference {
        index_incarnation: Some(old_incarnation),
        generation_no: 1,
        workspace_revision: "A".to_owned(),
        state: GenerationReferenceState::HistoricalMissingOrReused,
    };
    let missing = runtime
        .snapshot(uid, None)
        .expect("snapshot survives rebuild");
    assert_eq!(missing.baseline_generation, Some(historical.clone()));
    assert_eq!(missing.result_generation, Some(historical.clone()));
    assert_eq!(
        missing.working_state, before.working_state,
        "Working State kept"
    );
    assert_eq!(missing.result, before.result);

    // The new index publishes generation 1 again -- for revision B.
    assert_eq!(fixture.publish("B"), 1);
    let reused = runtime.snapshot(uid, None).expect("snapshot");
    assert_eq!(reused.baseline_generation, Some(historical.clone()));
    assert_eq!(reused.result_generation, Some(historical));
    let state = reused.working_state.expect("state");
    assert_eq!(
        (
            state.baseline_workspace_revision.as_str(),
            state.baseline_generation_no
        ),
        ("A", 1),
        "never rebound to the new generation 1"
    );

    // A BUILDING row with the stored incarnation, number and basis is not
    // a match either: the index below is given the *old* incarnation on
    // purpose so that only the BUILDING state differs.
    let rebuilt = TestDir::create("reuse-building");
    let building_index = rebuilt.path().join("index.db");
    {
        let index = GenerationStore::open(&building_index).expect("index");
        index.bootstrap_clock("A").expect("clock");
        index.begin_generation("A").expect("building gen 1 at A");
        Connection::open(&building_index)
            .expect("raw")
            .execute(
                "UPDATE db_meta SET workspace_uid = ?1, index_incarnation_uid = ?2 WHERE id = 0",
                params![
                    fixture.id.to_bytes().to_vec(),
                    old_incarnation.to_bytes().to_vec()
                ],
            )
            .expect("bind for the test");
    }
    let runtime = WorkRuntime::open(fixture.id, &fixture.paths.workspace_db, &building_index)
        .expect("runtime");
    assert_eq!(
        runtime
            .snapshot(uid, None)
            .expect("snapshot")
            .baseline_generation
            .expect("reference")
            .state,
        GenerationReferenceState::HistoricalMissingOrReused
    );
}

#[test]
fn a_result_while_indexing_catches_up_records_no_generation() {
    // #20 task 3 §21: completion does not wait for the index, and an older
    // stable generation is never labelled the result's generation.
    let fixture = Fixture::new("catching-up");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    fixture.advance("B");
    let done = runtime
        .complete(
            uid,
            &result("done", Some("c1"), DirtyObservation::Clean),
            None,
        )
        .expect("completion does not need a current generation");
    let stored = done.result.expect("result");
    assert_eq!(stored.result_workspace_revision, "B");
    assert_eq!(stored.result_generation_no, None);
    assert_eq!(stored.result_index_incarnation, None);
    assert_eq!(done.result_generation, None);
}

#[test]
fn snapshot_resources_are_bounded_not_truncated() {
    let fixture = Fixture::new("bound");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    let many: Vec<_> = (0..=RESOURCE_BOUND)
        .map(|_| evidence(ResourceId::generate(), WorkResourceRole::Related))
        .collect();
    runtime
        .update_progress(uid, &WorkProgress::default(), &many)
        .expect("progress");
    assert!(matches!(
        runtime.snapshot(uid, None),
        Err(WorkError::BoundExceeded {
            what: "Work Resources"
        })
    ));
}

// ======================== index incarnation (task 3 correction)

#[test]
fn same_number_and_same_revision_after_rebuild_stays_historical() {
    // The central correction case: the rebuilt index reproduces generation 1
    // at the very same revision A, and the old reference still must not
    // match it.
    let fixture = Fixture::new("same-revision-rebuild");
    assert_eq!(fixture.publish("A"), 1);
    let first = fixture.incarnation();
    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    runtime
        .record_partial(
            uid,
            &result("checkpoint", Some("abc"), DirtyObservation::Clean),
        )
        .expect("partial");
    let before = runtime.snapshot(uid, None).expect("snapshot");
    let reference = |state| GenerationReference {
        index_incarnation: Some(first),
        generation_no: 1,
        workspace_revision: "A".to_owned(),
        state,
    };
    assert_eq!(
        before.baseline_generation,
        Some(reference(GenerationReferenceState::PresentMatching))
    );
    assert_eq!(
        before.result_generation,
        Some(reference(GenerationReferenceState::PresentMatching))
    );
    let stored = before.result.clone().expect("result");
    assert_eq!(stored.result_index_incarnation, Some(first));
    drop(runtime);

    fixture.rebuild_index();
    let second = fixture.incarnation();
    assert_ne!(first, second, "a rebuilt index.db is a new incarnation");
    assert_eq!(fixture.publish("A"), 1, "same number, same revision");

    let runtime = fixture.runtime();
    let after = runtime.snapshot(uid, None).expect("snapshot");
    assert_eq!(
        after.baseline_generation,
        Some(reference(
            GenerationReferenceState::HistoricalMissingOrReused
        ))
    );
    assert_eq!(
        after.result_generation,
        Some(reference(
            GenerationReferenceState::HistoricalMissingOrReused
        ))
    );
    assert_eq!(after.working_state, before.working_state, "never rewritten");
    assert_eq!(after.result, before.result);

    // A new WorkItem in the rebuilt index binds to the new incarnation.
    let fresh = started(&runtime, "after rebuild");
    let fresh = runtime.snapshot(fresh, None).expect("snapshot");
    assert_eq!(
        fresh.baseline_generation,
        Some(GenerationReference {
            index_incarnation: Some(second),
            generation_no: 1,
            workspace_revision: "A".to_owned(),
            state: GenerationReferenceState::PresentMatching,
        })
    );
}

#[test]
fn reopening_the_same_index_keeps_its_incarnation() {
    // A restart is not a rebuild.
    let fixture = Fixture::new("reopen");
    fixture.publish("A");
    let first = fixture.incarnation();
    assert_eq!(fixture.incarnation(), first, "second open, same file");
    init_workspace(fixture.root.path(), &fixture.global).expect("repeated init reopens");
    assert_eq!(fixture.incarnation(), first);

    let runtime = fixture.runtime();
    let uid = started(&runtime, "g");
    drop(runtime);
    let runtime = fixture.runtime();
    assert_eq!(
        runtime
            .snapshot(uid, None)
            .expect("snapshot")
            .baseline_generation
            .expect("reference")
            .state,
        GenerationReferenceState::PresentMatching
    );
}

#[test]
fn independently_created_index_dbs_differ_for_one_workspace() {
    let fixture = Fixture::new("two-indexes");
    let other_dir = TestDir::create("two-indexes-other");
    let other = other_dir.path().join("index.db");
    drop(GenerationStore::open(&other).expect("second index"));
    Connection::open(&other)
        .expect("raw")
        .execute(
            "UPDATE db_meta SET workspace_uid = ?1 WHERE id = 0",
            params![fixture.id.to_bytes().to_vec()],
        )
        .expect("bind to the same Workspace");
    let first = fixture.index();
    let second = GenerationStore::open(&other).expect("second index");
    assert_eq!(
        first.bound_workspace_uid().expect("bound"),
        second.bound_workspace_uid().expect("bound"),
        "same WorkspaceID"
    );
    assert_ne!(
        first.index_incarnation_id().expect("first"),
        second.index_incarnation_id().expect("second"),
        "WorkspaceID is not the index incarnation"
    );
    WorkRuntime::open(fixture.id, &fixture.paths.workspace_db, &other)
        .expect("both are legitimately bound to this Workspace");
}

#[test]
fn index_v9_migrates_to_one_persistent_incarnation() {
    let dir = TestDir::create("index-v9");
    let path = dir.path().join("index.db");
    {
        let v9 = db::open(&path, DbKind::Index, &schema::index::INDEX_MIGRATIONS[..9])
            .expect("v9 index.db");
        assert_eq!(v9.schema_version, 9);
    }
    let migrated = GenerationStore::open(&path).expect("migrates to v10");
    let incarnation = migrated.index_incarnation_id().expect("initialized");
    drop(migrated);
    let reopened = schema::index::open(&path).expect("reopen");
    assert_eq!(reopened.schema_version, 10);
    let stored: Vec<u8> = reopened
        .connection
        .query_row(
            "SELECT index_incarnation_uid FROM db_meta WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .expect("stored");
    assert_eq!(stored, incarnation.to_bytes().to_vec());
    assert_eq!(
        GenerationStore::from_connection(reopened.connection)
            .index_incarnation_id()
            .expect("read"),
        incarnation
    );
}

#[test]
fn incarnation_reads_never_invent_a_value() {
    let dir = TestDir::create("incarnation-invalid");
    let path = dir.path().join("index.db");
    drop(GenerationStore::open(&path).expect("index"));
    let raw = Connection::open(&path).expect("raw");
    for bad in [
        rusqlite::types::Value::Blob(vec![7; 15]),
        rusqlite::types::Value::Text("0123456789abcdef".to_owned()),
    ] {
        assert!(
            raw.execute(
                "UPDATE db_meta SET index_incarnation_uid = ?1 WHERE id = 0",
                params![bad]
            )
            .is_err(),
            "CHECK rejects {bad:?}"
        );
    }
    raw.execute(
        "UPDATE db_meta SET index_incarnation_uid = NULL WHERE id = 0",
        [],
    )
    .expect("clear");
    let store = GenerationStore::open(&path).expect("reopen is not a migration");
    assert!(matches!(
        store.index_incarnation_id(),
        Err(GenerationError::InvalidIndexIncarnation { .. })
    ));
    assert!(
        matches!(
            store.index_incarnation_id(),
            Err(GenerationError::InvalidIndexIncarnation { .. })
        ),
        "a read does not fill it in"
    );
}

#[test]
fn workspace_v4_references_have_no_incarnation_and_stay_historical() {
    // Migration: pre-v5 rows get NULL, never the current index's value.
    let dir = TestDir::create("workspace-v4");
    let path = dir.path().join("workspace.db");
    let legacy = WorkItemId::generate();
    {
        let v4 = db::open(
            &path,
            DbKind::Workspace,
            &schema::workspace::WORKSPACE_MIGRATIONS[..4],
        )
        .expect("v4 workspace.db");
        insert_legacy_references(&v4.connection, legacy);
    }
    let store = WorkspaceKnowledgeStore::open(&path).expect("migrates to v5");
    let state = store
        .get_working_state(legacy)
        .expect("get")
        .expect("state");
    assert_eq!(
        (
            state.baseline_index_incarnation,
            state.baseline_generation_no
        ),
        (None, 1)
    );
    let result = store.get_work_result(legacy).expect("get").expect("result");
    assert_eq!(
        (result.result_index_incarnation, result.result_generation_no),
        (None, Some(1))
    );
    drop(store);

    // Behaviour: the same legacy shape in a bound Workspace whose current
    // index really has STABLE generation 1 at revision A.
    let fixture = Fixture::new("legacy-historical");
    assert_eq!(fixture.publish("A"), 1);
    let connection = Connection::open(&fixture.paths.workspace_db).expect("raw");
    insert_legacy_references(&connection, legacy);
    let snapshot = fixture.runtime().snapshot(legacy, None).expect("snapshot");
    let historical = Some(GenerationReference {
        index_incarnation: None,
        generation_no: 1,
        workspace_revision: "A".to_owned(),
        state: GenerationReferenceState::HistoricalMissingOrReused,
    });
    assert_eq!(snapshot.baseline_generation, historical);
    assert_eq!(snapshot.result_generation, historical);
    assert_eq!(
        snapshot
            .working_state
            .expect("state")
            .baseline_index_incarnation,
        None,
        "not backfilled on read"
    );
}

/// An ACTIVE WorkItem with a baseline and a result at generation 1 @ A,
/// written without any incarnation -- the pre-correction row shape.
fn insert_legacy_references(connection: &Connection, uid: WorkItemId) {
    let uid = uid.to_bytes().to_vec();
    connection
        .execute(
            "INSERT INTO work_item (uid, source_kind, goal, status, created_at) \
             VALUES (?1, 'ISSUE', 'legacy', 'ACTIVE', '0')",
            params![uid],
        )
        .expect("item");
    connection
        .execute(
            "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
             baseline_generation_no, last_observed_workspace_revision, updated_at) \
             SELECT id, 'A', 1, 'A', '0' FROM work_item WHERE uid = ?1",
            params![uid],
        )
        .expect("state");
    connection
        .execute(
            "INSERT INTO work_result (work_item_id, result_status, result_summary, \
             result_workspace_revision, result_generation_no, created_at) \
             SELECT id, 'PARTIAL', 'legacy', 'A', 1, '0' FROM work_item WHERE uid = ?1",
            params![uid],
        )
        .expect("result");
}

#[test]
fn new_generation_references_always_carry_their_incarnation() {
    let fixture = Fixture::new("write-invariant");
    fixture.publish("A");
    let runtime = fixture.runtime();
    let uid = runtime.create(&item("g")).expect("create").uid;
    let store = WorkspaceKnowledgeStore::open(&fixture.paths.workspace_db).expect("store");
    let unverifiable = WorkingState {
        work_item: uid,
        baseline_workspace_revision: "A".to_owned(),
        baseline_index_incarnation: None,
        baseline_generation_no: 1,
        baseline_head: None,
        baseline_dirty: DirtyObservation::Unknown,
        current_step: None,
        progress_summary: None,
        remaining_summary: None,
        blocker_summary: None,
        owner_agent: None,
        last_observed_workspace_revision: "A".to_owned(),
        updated_at: String::new(),
    };
    assert!(store.insert_working_state(&unverifiable).is_err());
    let unpaired = |incarnation, generation_no| WorkResult {
        work_item: uid,
        result_status: WorkResultStatus::Partial,
        result_summary: "p".to_owned(),
        commit_id: None,
        change_set_fingerprint: None,
        verification_summary: None,
        result_workspace_revision: "A".to_owned(),
        result_index_incarnation: incarnation,
        result_generation_no: generation_no,
        remaining_dirty: DirtyObservation::Unknown,
        created_at: String::new(),
    };
    assert!(store.record_work_result(&unpaired(None, Some(1))).is_err());
    assert!(
        store
            .record_work_result(&unpaired(Some(IndexIncarnationId::generate()), None))
            .is_err()
    );

    runtime.start(uid, &start_unknown()).expect("start");
    let state = runtime
        .snapshot(uid, None)
        .expect("s")
        .working_state
        .expect("state");
    assert_eq!(
        state.baseline_index_incarnation,
        Some(fixture.incarnation())
    );

    // Schema-level backstop for the same pairing and the 16-byte shape.
    let raw = Connection::open(&fixture.paths.workspace_db).expect("raw");
    for (incarnation, generation_no) in [
        (rusqlite::types::Value::Blob(vec![1; 16]), None),
        (rusqlite::types::Value::Blob(vec![1; 15]), Some(1_i64)),
    ] {
        assert!(
            raw.execute(
                "INSERT INTO work_result (work_item_id, result_status, result_summary, \
                 result_workspace_revision, result_generation_no, result_index_incarnation_uid, \
                 created_at) SELECT id, 'PARTIAL', 'x', 'A', ?2, ?1, '0' FROM work_item \
                 WHERE uid = ?3",
                params![incarnation, generation_no, uid.to_bytes().to_vec()],
            )
            .is_err()
        );
    }
    assert!(
        raw.execute(
            "UPDATE working_state SET baseline_index_incarnation_uid = ?1",
            params![vec![1_u8; 15]],
        )
        .is_err()
    );
}

// ============================================ boundaries (49, 50)

#[test]
fn lifecycle_source_runs_no_command_and_needs_no_backend() {
    let source = include_str!("../work.rs");
    let code = &source[..source.find("#[cfg(test)]").expect("test module marker")];
    for forbidden in [
        "Command",
        "std::process",
        "\"git",
        "semantic",
        "lsp",
        "crate::refresh",
        "crate::scan",
        "crate::watch",
        "init_workspace",
        "thread::sleep",
    ] {
        assert!(
            !code.contains(forbidden),
            "work.rs must not reference {forbidden}"
        );
    }
}

// ================================================= EXPLAIN QUERY PLAN

fn plan(connection: &Connection, sql: &str, params: impl rusqlite::Params) -> String {
    let mut statement = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("plan prepares");
    let rows: Vec<String> = statement
        .query_map(params, |row| row.get::<_, String>(3))
        .expect("plan runs")
        .collect::<Result<_, _>>()
        .expect("plan decodes");
    rows.join(" | ")
}

/// Task 3 SQL access plan (#20): T6 overlap, T8 latest handoff, T9
/// generation by number. Run with `--nocapture` to print the plans.
#[test]
fn task3_access_paths_use_their_intended_indexes() {
    let fixture = Fixture::new("eqp");
    let workspace = Connection::open(&fixture.paths.workspace_db).expect("workspace");
    let index = Connection::open(&fixture.paths.index_db).expect("index");
    let cases: [(&str, &Connection, &str, &[&str]); 4] = [
        (
            "C1 index incarnation",
            &index,
            "SELECT index_incarnation_uid FROM db_meta WHERE id = ?1 LIMIT ?2",
            &["SEARCH db_meta USING INTEGER PRIMARY KEY (rowid=?)"],
        ),
        (
            "T6 overlap",
            &workspace,
            OVERLAP_SQL,
            &[
                "idx_work_resource_resource_role (resource_uid=? AND role=?)",
                "USING INTEGER PRIMARY KEY",
            ],
        ),
        (
            "T8 latest handoff",
            &workspace,
            "SELECT h.handoff_summary FROM work_handoff h JOIN work_item w ON w.id = h.work_item_id \
             WHERE h.work_item_id = ?1 ORDER BY h.id DESC LIMIT ?2",
            &["idx_work_handoff_work_item"],
        ),
        (
            "T9 generation by number",
            &index,
            "SELECT id FROM generation WHERE generation_no = ?1 LIMIT ?2",
            &["sqlite_autoindex_generation_1"],
        ),
    ];
    for (label, connection, sql, expected) in cases {
        let found = plan(connection, sql, params![1_i64, 1_i64]);
        println!("{label}: {found}");
        for index in expected {
            assert!(
                found.contains(index),
                "{label}: expected {index}, got {found}"
            );
        }
        assert!(
            !found.split(" | ").any(|step| step.starts_with("SCAN ")),
            "{label}: unexpected full scan in {found}"
        );
    }
    // T6 drives from the requesting item's own rows, not from every
    // non-terminal WorkItem.
    let overlap = plan(&workspace, OVERLAP_SQL, params![1_i64, 1_i64]);
    assert!(
        overlap.starts_with(
            "SEARCH s USING COVERING INDEX sqlite_autoindex_work_resource_1 (work_item_id=?)"
        ),
        "{overlap}"
    );
}
