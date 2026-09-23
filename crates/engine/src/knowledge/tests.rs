//! #20 task 1 acceptance: typed model, three stores, migrations from the
//! pre-I5 schema, worktree sharing/isolation, index-rebuild durability,
//! and the query plans behind the task 1 SQL access plan.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use brainprint_core::{ProjectId, ResourceId, WorkspaceId};
use rusqlite::{Connection, params};
use serde_json::json;

use super::*;
use crate::{
    db::{self, DbKind},
    init::init_workspace,
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
            "brainprint-knowledge-{label}-{}-{sequence}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("test directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn db(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn user_explicit() -> Provenance {
    Provenance::new(SourceKind::UserExplicit)
        .with_locator("issue#20")
        .with_revision("rev-1")
}

fn new_policy(scope: KnowledgeScope, title: &str) -> NewPolicy {
    NewPolicy {
        scope,
        policy_key: Some(format!("key-{title}")),
        title: title.to_owned(),
        rule_text: format!("rule for {title}"),
        structured_rule: Some(json!({ "must_not": "create issues unrequested" })),
        protection_class: ProtectionClass::Normal,
        priority_class: PriorityClass::Default,
        provenance: user_explicit(),
    }
}

fn new_decision(topic: &str) -> NewDecision {
    NewDecision {
        scope: KnowledgeScope::project(),
        topic: topic.to_owned(),
        chosen_summary: format!("{topic} chosen"),
        rationale: "because".to_owned(),
        provenance: user_explicit(),
    }
}

fn definition() -> BlueprintDefinition {
    BlueprintDefinition {
        components: vec![
            BlueprintComponent {
                name: "structural".to_owned(),
                description: Some("Tree-sitter tier".to_owned()),
            },
            BlueprintComponent {
                name: "semantic".to_owned(),
                description: None,
            },
        ],
        relationships: vec![BlueprintRelationship {
            from: "semantic".to_owned(),
            to: "structural".to_owned(),
            kind: "augments".to_owned(),
            description: None,
        }],
        constraints: vec!["semantic never overwrites structural truth".to_owned()],
    }
}

fn new_blueprint(scope: KnowledgeScope, status: BlueprintStatus) -> NewBlueprint {
    NewBlueprint {
        scope,
        blueprint_key: Some("language-intelligence".to_owned()),
        title: "Language Intelligence".to_owned(),
        intent: "Structural + Semantic + Adapter".to_owned(),
        definition: definition(),
        status,
        version: Some("1".to_owned()),
        provenance: user_explicit(),
    }
}

fn state(key: &str, scope: KnowledgeScope, value: TypedValue) -> ProjectStateUpdate {
    ProjectStateUpdate {
        key: key.to_owned(),
        scope,
        value,
        status: ProjectStateStatus::Current,
        provenance: Provenance::new(SourceKind::Observed).with_revision("gen-7"),
    }
}

fn new_work_item(goal: &str) -> NewWorkItem {
    NewWorkItem {
        source_kind: WorkItemSourceKind::Issue,
        source_ref: Some("#20".to_owned()),
        title: Some("I5 task 1".to_owned()),
        goal: goal.to_owned(),
    }
}

fn working_state(work_item: WorkItemId, step: &str) -> WorkingState {
    WorkingState {
        work_item,
        baseline_workspace_revision: "r1".to_owned(),
        baseline_generation_no: 3,
        baseline_head: Some("550dbe3".to_owned()),
        baseline_dirty_fingerprint: None,
        current_step: Some(step.to_owned()),
        progress_summary: Some("schema done".to_owned()),
        remaining_summary: Some("tests".to_owned()),
        blocker_summary: None,
        owner_agent: Some("claude".to_owned()),
        last_observed_workspace_revision: "r2".to_owned(),
        updated_at: String::new(),
    }
}

fn is_unknown(error: &KnowledgeError, vocabulary: &str) -> bool {
    matches!(error, KnowledgeError::UnknownValue { vocabulary: found, .. } if *found == vocabulary)
}

fn table_names(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("table listing should prepare");
    statement
        .query_map([], |row| row.get(0))
        .expect("table listing should run")
        .collect::<Result<_, _>>()
        .expect("table names should decode")
}

// ================================================================ global

#[test]
fn user_policy_round_trips() {
    let dir = TestDir::create("user-policy");
    let store = GlobalKnowledgeStore::open(&dir.db("global.db")).expect("global.db opens");

    let created = store
        .insert_user_policy(&new_policy(KnowledgeScope::global(), "no-new-issues"))
        .expect("insert");
    assert_eq!(created.status, PolicyStatus::Active);
    assert_eq!(
        store.get_user_policy(created.uid).expect("get"),
        Some(created.clone())
    );
    assert_eq!(created.provenance, user_explicit());
    assert_eq!(
        created.structured_rule,
        Some(json!({ "must_not": "create issues unrequested" }))
    );
    assert_eq!(
        store
            .list_user_policies(&KnowledgeScope::global(), PolicyStatus::Active, 10)
            .expect("list"),
        vec![created]
    );
}

#[test]
fn user_policy_unknown_status_is_rejected() {
    let dir = TestDir::create("user-policy-status");
    let path = dir.db("global.db");
    let store = GlobalKnowledgeStore::open(&path).expect("global.db opens");
    let created = store
        .insert_user_policy(&new_policy(KnowledgeScope::global(), "p"))
        .expect("insert");

    Connection::open(&path)
        .expect("raw connection")
        .execute("UPDATE user_policy SET status = 'PROPOSED'", [])
        .expect("raw update");

    let error = store
        .get_user_policy(created.uid)
        .expect_err("PROPOSED is not a Policy status");
    assert!(is_unknown(&error, "PolicyStatus"), "{error:?}");
}

#[test]
fn user_policy_supersedes_lineage_is_retained() {
    let dir = TestDir::create("user-policy-lineage");
    let path = dir.db("global.db");
    let (old, new) = {
        let store = GlobalKnowledgeStore::open(&path).expect("global.db opens");
        let old = store
            .insert_user_policy(&new_policy(KnowledgeScope::global(), "old"))
            .expect("old");
        let new = store
            .insert_user_policy(&new_policy(KnowledgeScope::global(), "new"))
            .expect("new");
        store
            .supersede_user_policy(new.uid, old.uid)
            .expect("supersede");
        (old, new)
    };

    let store = GlobalKnowledgeStore::open(&path).expect("reopen");
    assert_eq!(
        store
            .get_user_policy(old.uid)
            .expect("get")
            .map(|p| p.status),
        Some(PolicyStatus::Superseded)
    );
    let lineage = store.user_policy_lineage(new.uid).expect("lineage");
    assert_eq!(lineage.supersedes, vec![old.uid]);
    assert!(lineage.superseded_by.is_empty());
    assert_eq!(
        store
            .user_policy_lineage(old.uid)
            .expect("lineage")
            .superseded_by,
        vec![new.uid]
    );
}

#[test]
fn user_preference_typed_value_round_trips() {
    let dir = TestDir::create("preference");
    let store = GlobalKnowledgeStore::open(&dir.db("global.db")).expect("global.db opens");

    for (key, value) in [
        ("language", TypedValue::Text("ko".to_owned())),
        ("max_width", TypedValue::Integer(100)),
        ("compact", TypedValue::Boolean(true)),
        (
            "format",
            TypedValue::Json(json!({ "direct_result_first": true })),
        ),
    ] {
        let created = store
            .insert_user_preference(&NewUserPreference {
                scope: KnowledgeScope::global(),
                preference_key: key.to_owned(),
                value: value.clone(),
                provenance: user_explicit(),
            })
            .expect("insert");
        assert_eq!(created.value, value);
        assert_eq!(
            store.get_user_preference(created.uid).expect("get"),
            Some(created.clone())
        );
        assert_eq!(
            store
                .find_user_preferences(&KnowledgeScope::global(), key, PreferenceStatus::Active, 5)
                .expect("find"),
            vec![created]
        );
    }
    assert_eq!(
        store
            .list_user_preferences(&KnowledgeScope::global(), PreferenceStatus::Active, 10)
            .expect("list")
            .iter()
            .map(|p| p.preference_key.as_str())
            .collect::<Vec<_>>(),
        vec!["compact", "format", "language", "max_width"]
    );
}

#[test]
fn reusable_blueprint_definition_round_trips_and_is_validated() {
    let dir = TestDir::create("global-blueprint");
    let path = dir.db("global.db");
    let store = GlobalKnowledgeStore::open(&path).expect("global.db opens");

    let created = store
        .insert_blueprint(&new_blueprint(
            KnowledgeScope::global(),
            BlueprintStatus::Draft,
        ))
        .expect("insert");
    assert_eq!(created.definition, definition());
    assert_eq!(
        store.get_blueprint(created.uid).expect("get"),
        Some(created.clone())
    );
    assert_eq!(
        store
            .set_blueprint_status(created.uid, BlueprintStatus::Active)
            .expect("activate")
            .status,
        BlueprintStatus::Active
    );
    assert!(
        store
            .set_blueprint_status(created.uid, BlueprintStatus::Draft)
            .is_err()
    );

    let mut broken = new_blueprint(KnowledgeScope::global(), BlueprintStatus::Draft);
    broken.definition.relationships[0].to = "undeclared".to_owned();
    assert!(matches!(
        store.insert_blueprint(&broken),
        Err(KnowledgeError::InvalidJson { .. })
    ));
    assert!(
        store
            .insert_blueprint(&new_blueprint(
                KnowledgeScope::global(),
                BlueprintStatus::Retired
            ))
            .is_err()
    );

    Connection::open(&path)
        .expect("raw connection")
        .execute(
            "UPDATE blueprint SET definition_json = '{\"components\":[],\"memo\":\"x\"}'",
            [],
        )
        .expect("raw update");
    assert!(matches!(
        store.get_blueprint(created.uid),
        Err(KnowledgeError::InvalidJson { .. })
    ));
}

#[test]
fn global_store_rejects_project_bound_scope() {
    let dir = TestDir::create("global-scope");
    let store = GlobalKnowledgeStore::open(&dir.db("global.db")).expect("global.db opens");
    let workspace = KnowledgeScope::workspace(WorkspaceId::generate());

    assert!(matches!(
        store.insert_user_policy(&new_policy(workspace, "p")),
        Err(KnowledgeError::InvalidScope { .. })
    ));
    assert!(
        store
            .insert_user_policy(&new_policy(
                KnowledgeScope::keyed(ScopeKind::Domain, "python").expect("scope"),
                "p"
            ))
            .is_ok()
    );
}

#[test]
fn registry_rows_survive_knowledge_migration() {
    let dir = TestDir::create("global-upgrade");
    let path = dir.db("global.db");
    let project_id = ProjectId::generate();
    {
        let pre_i5 = db::open(
            &path,
            DbKind::Global,
            &schema::global::GLOBAL_MIGRATIONS[..3],
        )
        .expect("pre-I5 v3 global.db");
        assert_eq!(pre_i5.schema_version, 3);
        pre_i5
            .connection
            .execute(
                "INSERT INTO project_registry (project_uid, home_locator, created_at, updated_at) \
                 VALUES (?1, '/repo/main', '0', '0')",
                params![project_id.to_bytes().to_vec()],
            )
            .expect("pre-I5 registry row");
    }

    let registry = GlobalRegistry::open(&path).expect("registry opens and migrates");
    let project = registry
        .get_project(project_id)
        .expect("lookup")
        .expect("pre-I5 project survives");
    assert_eq!(project.home_locator, PathBuf::from("/repo/main"));

    let opened = schema::global::open(&path).expect("reopen");
    assert_eq!(opened.schema_version, 4);
}

#[test]
fn knowledge_rows_survive_registry_reopen() {
    let dir = TestDir::create("global-reopen");
    let path = dir.db("global.db");
    let uid = GlobalKnowledgeStore::open(&path)
        .expect("store")
        .insert_user_policy(&new_policy(KnowledgeScope::global(), "p"))
        .expect("insert")
        .uid;

    drop(GlobalRegistry::open(&path).expect("registry"));
    drop(GlobalRegistry::open(&path).expect("registry again"));

    let store = GlobalKnowledgeStore::open(&path).expect("store reopen");
    assert!(store.get_user_policy(uid).expect("get").is_some());
}

// =============================================================== project

#[test]
fn existing_policy_row_is_readable_after_migration() {
    let dir = TestDir::create("project-upgrade-policy");
    let path = dir.db("project.db");
    let uid = PolicyId::generate();
    {
        let pre_i5 = db::open(
            &path,
            DbKind::Project,
            &schema::project::PROJECT_MIGRATIONS[..2],
        )
        .expect("pre-I5 v2 project.db");
        pre_i5
            .connection
            .execute(
                "INSERT INTO policy (uid, scope_kind, title, rule_text, protection_class, \
                 priority_class, status, source_kind, created_at, updated_at) \
                 VALUES (?1, 'PROJECT', 'legacy', 'r', 'NORMAL', 'DEFAULT', 'ACTIVE', \
                 'USER_EXPLICIT', '0', '0')",
                params![uid.to_bytes().to_vec()],
            )
            .expect("pre-I5 policy row");
    }

    let store = ProjectKnowledgeStore::open(&path).expect("migrates to v3");
    let policy = store
        .get_policy(uid)
        .expect("get")
        .expect("legacy row survives");
    assert_eq!(policy.title, "legacy");
    assert_eq!(policy.scope, KnowledgeScope::project());
    assert!(
        store
            .policy_lineage(uid)
            .expect("lineage")
            .supersedes
            .is_empty()
    );
}

#[test]
fn typed_policy_create_read_and_status() {
    let dir = TestDir::create("project-policy");
    let store = ProjectKnowledgeStore::open(&dir.db("project.db")).expect("project.db opens");
    let created = store
        .insert_policy(&new_policy(KnowledgeScope::project(), "p"))
        .expect("insert");
    assert_eq!(
        store.get_policy(created.uid).expect("get"),
        Some(created.clone())
    );

    let disabled = store
        .set_policy_status(created.uid, PolicyStatus::Disabled)
        .expect("disable");
    assert_eq!(disabled.status, PolicyStatus::Disabled);
    assert!(
        store
            .set_policy_status(created.uid, PolicyStatus::Superseded)
            .is_err()
    );
    assert_eq!(
        store
            .set_policy_status(created.uid, PolicyStatus::Active)
            .expect("re-enable")
            .status,
        PolicyStatus::Active
    );
    assert!(matches!(
        store.insert_policy(&new_policy(KnowledgeScope::global(), "g")),
        Err(KnowledgeError::InvalidScope { .. })
    ));
}

#[test]
fn workspace_scoped_policy_stores_exact_workspace_id() {
    let dir = TestDir::create("project-workspace-policy");
    let store = ProjectKnowledgeStore::open(&dir.db("project.db")).expect("project.db opens");
    let workspace_id = WorkspaceId::generate();
    let scope = KnowledgeScope::workspace(workspace_id);

    let created = store
        .insert_policy(&new_policy(scope.clone(), "wt"))
        .expect("insert");
    assert_eq!(created.scope.kind(), ScopeKind::Workspace);
    assert_eq!(created.scope.key(), Some(workspace_id.to_string().as_str()));
    assert_eq!(
        store
            .list_policies(&scope, PolicyStatus::Active, 10)
            .expect("list"),
        vec![created]
    );
    assert!(
        store
            .list_policies(&KnowledgeScope::project(), PolicyStatus::Active, 10)
            .expect("list")
            .is_empty()
    );
    assert!(
        store
            .list_policies(
                &KnowledgeScope::workspace(WorkspaceId::generate()),
                PolicyStatus::Active,
                10
            )
            .expect("list")
            .is_empty()
    );
}

#[test]
fn policy_supersedes_link_survives_reopen_and_is_atomic() {
    let dir = TestDir::create("project-policy-lineage");
    let path = dir.db("project.db");
    let (old, new, third) = {
        let store = ProjectKnowledgeStore::open(&path).expect("project.db opens");
        let old = store
            .insert_policy(&new_policy(KnowledgeScope::project(), "old"))
            .expect("old");
        let new = store
            .insert_policy(&new_policy(KnowledgeScope::project(), "new"))
            .expect("new");
        let third = store
            .insert_policy(&new_policy(KnowledgeScope::project(), "third"))
            .expect("third");
        store.supersede_policy(new.uid, old.uid).expect("supersede");
        (old, new, third)
    };

    let store = ProjectKnowledgeStore::open(&path).expect("reopen");
    assert_eq!(
        store.get_policy(old.uid).expect("get").map(|p| p.status),
        Some(PolicyStatus::Superseded)
    );
    assert_eq!(
        store.policy_lineage(new.uid).expect("lineage").supersedes,
        vec![old.uid]
    );
    assert_eq!(
        store
            .policy_lineage(old.uid)
            .expect("lineage")
            .superseded_by,
        vec![new.uid]
    );

    // Superseding an already-SUPERSEDED row fails and writes nothing.
    assert!(store.supersede_policy(third.uid, old.uid).is_err());
    assert!(
        store
            .policy_lineage(third.uid)
            .expect("lineage")
            .supersedes
            .is_empty()
    );
    assert_eq!(
        store.get_policy(third.uid).expect("get").map(|p| p.status),
        Some(PolicyStatus::Active)
    );
    assert!(store.supersede_policy(new.uid, new.uid).is_err());
    assert!(
        store
            .set_policy_status(old.uid, PolicyStatus::Active)
            .is_err()
    );
}

#[test]
fn decision_lineage_works_before_and_after_migration() {
    let dir = TestDir::create("project-decision");
    let path = dir.db("project.db");
    let (legacy_old, legacy_new) = (DecisionId::generate(), DecisionId::generate());
    {
        let pre_i5 = db::open(
            &path,
            DbKind::Project,
            &schema::project::PROJECT_MIGRATIONS[..2],
        )
        .expect("pre-I5 v2 project.db");
        for (uid, status) in [(legacy_old, "SUPERSEDED"), (legacy_new, "ACTIVE")] {
            pre_i5
                .connection
                .execute(
                    "INSERT INTO decision (uid, scope_kind, topic, chosen_summary, rationale, \
                     status, source_kind, created_at, updated_at) \
                     VALUES (?1, 'PROJECT', 'baseline', 'c', 'r', ?2, 'USER_EXPLICIT', '0', '0')",
                    params![uid.to_bytes().to_vec(), status],
                )
                .expect("pre-I5 decision");
        }
        pre_i5
            .connection
            .execute(
                "INSERT INTO decision_link (decision_id, related_decision_id, link_kind) \
                 SELECT n.id, o.id, 'SUPERSEDES' FROM decision n, decision o \
                 WHERE n.uid = ?1 AND o.uid = ?2",
                params![
                    legacy_new.to_bytes().to_vec(),
                    legacy_old.to_bytes().to_vec()
                ],
            )
            .expect("pre-I5 link");
    }

    let store = ProjectKnowledgeStore::open(&path).expect("migrates");
    assert_eq!(
        store
            .decision_lineage(legacy_new)
            .expect("lineage")
            .outgoing,
        vec![DecisionLink {
            kind: DecisionLinkKind::Supersedes,
            other: legacy_old
        }]
    );

    let reverser = store
        .insert_decision(&new_decision("baseline"))
        .expect("insert");
    store
        .reverse_decision(reverser.uid, legacy_new)
        .expect("reverse");
    assert_eq!(
        store
            .get_decision(legacy_new)
            .expect("get")
            .map(|d| d.status),
        Some(DecisionStatus::Reversed)
    );
    assert_eq!(
        store
            .decision_lineage(legacy_new)
            .expect("lineage")
            .incoming,
        vec![DecisionLink {
            kind: DecisionLinkKind::Reverses,
            other: reverser.uid
        }]
    );
    assert_eq!(
        store
            .list_decisions_by_topic("baseline", DecisionStatus::Active, 10)
            .expect("list")
            .iter()
            .map(|d| d.uid)
            .collect::<Vec<_>>(),
        vec![reverser.uid]
    );
    let replacement = store
        .insert_decision(&new_decision("baseline"))
        .expect("insert");
    store
        .supersede_decision(replacement.uid, reverser.uid)
        .expect("supersede");
    assert_eq!(
        store
            .get_decision(reverser.uid)
            .expect("get")
            .map(|d| d.status),
        Some(DecisionStatus::Superseded)
    );
}

#[test]
fn project_local_blueprint_round_trips() {
    let dir = TestDir::create("project-blueprint");
    let store = ProjectKnowledgeStore::open(&dir.db("project.db")).expect("project.db opens");
    let created = store
        .insert_blueprint(&new_blueprint(
            KnowledgeScope::project(),
            BlueprintStatus::Active,
        ))
        .expect("insert");
    assert_eq!(
        store.get_blueprint(created.uid).expect("get"),
        Some(created.clone())
    );
    assert_eq!(
        store
            .list_blueprints(&KnowledgeScope::project(), BlueprintStatus::Active, 10)
            .expect("list"),
        vec![created]
    );
}

#[test]
fn blueprint_application_discriminates_owner() {
    let dir = TestDir::create("project-application");
    let store = ProjectKnowledgeStore::open(&dir.db("project.db")).expect("project.db opens");
    let global_uid = BlueprintId::generate();
    let local = store
        .insert_blueprint(&new_blueprint(
            KnowledgeScope::project(),
            BlueprintStatus::Active,
        ))
        .expect("local blueprint");

    let global_app = store
        .insert_blueprint_application(&NewBlueprintApplication {
            blueprint: BlueprintRef {
                owner: BlueprintOwnerKind::Global,
                uid: global_uid,
            },
            scope: KnowledgeScope::project(),
            application_summary: "applies the reusable layering".to_owned(),
            provenance: user_explicit(),
        })
        .expect("GLOBAL reference needs no local row");
    assert_eq!(global_app.blueprint.owner, BlueprintOwnerKind::Global);
    assert_eq!(global_app.blueprint.uid, global_uid);
    assert_eq!(global_app.provenance, user_explicit());

    let project_app = store
        .insert_blueprint_application(&NewBlueprintApplication {
            blueprint: BlueprintRef {
                owner: BlueprintOwnerKind::Project,
                uid: local.uid,
            },
            scope: KnowledgeScope::project(),
            application_summary: "applies the local blueprint".to_owned(),
            provenance: user_explicit(),
        })
        .expect("PROJECT reference to an existing local blueprint");
    assert_eq!(project_app.blueprint.owner, BlueprintOwnerKind::Project);

    assert!(matches!(
        store.insert_blueprint_application(&NewBlueprintApplication {
            blueprint: BlueprintRef {
                owner: BlueprintOwnerKind::Project,
                uid: BlueprintId::generate(),
            },
            scope: KnowledgeScope::project(),
            application_summary: "dangling".to_owned(),
            provenance: user_explicit(),
        }),
        Err(KnowledgeError::NotFound { .. })
    ));

    assert_eq!(
        store
            .list_blueprint_applications(
                &KnowledgeScope::project(),
                BlueprintApplicationStatus::Active,
                10
            )
            .expect("list")
            .len(),
        2
    );
    let retired = store
        .set_blueprint_application_status(global_app.uid, BlueprintApplicationStatus::Retired)
        .expect("retire");
    assert_eq!(retired.status, BlueprintApplicationStatus::Retired);
    assert!(
        store
            .set_blueprint_application_status(global_app.uid, BlueprintApplicationStatus::Active)
            .is_err()
    );
}

#[test]
fn pre_i5_blueprint_application_migrates_as_global() {
    let dir = TestDir::create("project-upgrade-application");
    let path = dir.db("project.db");
    let (application_uid, blueprint_uid) =
        (BlueprintApplicationId::generate(), BlueprintId::generate());
    {
        let pre_i5 = db::open(
            &path,
            DbKind::Project,
            &schema::project::PROJECT_MIGRATIONS[..2],
        )
        .expect("pre-I5 v2 project.db");
        pre_i5
            .connection
            .execute(
                "INSERT INTO blueprint_application (uid, blueprint_uid, scope_kind, status, \
                 application_summary, source_kind, created_at, updated_at) \
                 VALUES (?1, ?2, 'PROJECT', 'ACTIVE', 'legacy', 'USER_EXPLICIT', '0', '0')",
                params![
                    application_uid.to_bytes().to_vec(),
                    blueprint_uid.to_bytes().to_vec()
                ],
            )
            .expect("pre-I5 application");
    }

    let store = ProjectKnowledgeStore::open(&path).expect("migrates");
    let application = store
        .get_blueprint_application(application_uid)
        .expect("get")
        .expect("legacy application survives");
    assert_eq!(
        application.blueprint,
        BlueprintRef {
            owner: BlueprintOwnerKind::Global,
            uid: blueprint_uid
        }
    );
    assert_eq!(application.provenance.revision, None);
}

#[test]
fn project_state_typed_value_round_trips_and_upsert_keeps_identity() {
    let dir = TestDir::create("project-state");
    let store = ProjectKnowledgeStore::open(&dir.db("project.db")).expect("project.db opens");
    let module = KnowledgeScope::keyed(ScopeKind::Module, "engine").expect("scope");

    let first = store
        .upsert_project_state(&state(
            "milestone",
            KnowledgeScope::project(),
            TypedValue::Text("I4".into()),
        ))
        .expect("insert");
    let second = store
        .upsert_project_state(&state(
            "milestone",
            KnowledgeScope::project(),
            TypedValue::Text("I5".into()),
        ))
        .expect("update keyless scope");
    assert_eq!(first.uid, second.uid);
    assert_eq!(second.value, TypedValue::Text("I5".to_owned()));
    assert_eq!(second.provenance.revision.as_deref(), Some("gen-7"));

    let keyed_first = store
        .upsert_project_state(&state("tasks", module.clone(), TypedValue::Integer(14)))
        .expect("insert keyed");
    let keyed_second = store
        .upsert_project_state(&state(
            "tasks",
            module.clone(),
            TypedValue::Json(json!({ "done": 1 })),
        ))
        .expect("update keyed scope");
    assert_eq!(keyed_first.uid, keyed_second.uid);
    assert_eq!(keyed_second.value, TypedValue::Json(json!({ "done": 1 })));

    assert_eq!(
        store
            .get_project_state("milestone", &KnowledgeScope::project())
            .expect("get"),
        Some(second)
    );
    assert_eq!(
        store
            .list_project_state(&KnowledgeScope::project(), 10)
            .expect("list")
            .len(),
        1
    );
    assert_eq!(
        store.list_project_state(&module, 10).expect("list").len(),
        1
    );
}

#[test]
fn pre_i5_project_state_gets_a_stable_uid() {
    let dir = TestDir::create("project-upgrade-state");
    let path = dir.db("project.db");
    {
        let pre_i5 = db::open(
            &path,
            DbKind::Project,
            &schema::project::PROJECT_MIGRATIONS[..2],
        )
        .expect("pre-I5 v2 project.db");
        pre_i5
            .connection
            .execute(
                "INSERT INTO project_state (state_key, scope_kind, value_type, value_json, status, \
                 source_kind, updated_at) \
                 VALUES ('milestone', 'PROJECT', 'TEXT', '\"I4\"', 'CURRENT', 'OBSERVED', '0')",
                [],
            )
            .expect("pre-I5 state");
    }
    let store = ProjectKnowledgeStore::open(&path).expect("migrates");
    let legacy = store
        .get_project_state("milestone", &KnowledgeScope::project())
        .expect("get")
        .expect("legacy state survives");
    let updated = store
        .upsert_project_state(&state(
            "milestone",
            KnowledgeScope::project(),
            TypedValue::Text("I5".into()),
        ))
        .expect("upsert");
    assert_eq!(updated.uid, legacy.uid);
}

// ============================================================= workspace

#[test]
fn existing_work_item_and_working_state_survive_migration() {
    let dir = TestDir::create("workspace-upgrade");
    let path = dir.db("workspace.db");
    let uid = WorkItemId::generate();
    {
        let pre_i5 = db::open(
            &path,
            DbKind::Workspace,
            &schema::workspace::WORKSPACE_MIGRATIONS[..2],
        )
        .expect("pre-I5 v2 workspace.db");
        pre_i5
            .connection
            .execute(
                "INSERT INTO work_item (uid, source_kind, source_ref, goal, status, created_at) \
                 VALUES (?1, 'ISSUE', '#19', 'close I4', 'ACTIVE', '0')",
                params![uid.to_bytes().to_vec()],
            )
            .expect("pre-I5 work item");
        pre_i5
            .connection
            .execute(
                "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
                 baseline_generation_no, current_step, last_observed_workspace_revision, updated_at) \
                 SELECT id, 'r0', 1, 'task 15', 'r1', '0' FROM work_item WHERE uid = ?1",
                params![uid.to_bytes().to_vec()],
            )
            .expect("pre-I5 working state");
    }

    let store = WorkspaceKnowledgeStore::open(&path).expect("migrates");
    let item = store
        .get_work_item(uid)
        .expect("get")
        .expect("legacy item survives");
    assert_eq!(item.status, WorkItemStatus::Active);
    let snapshot = store
        .get_working_state(uid)
        .expect("get")
        .expect("legacy state survives");
    assert_eq!(snapshot.current_step.as_deref(), Some("task 15"));
    assert_eq!(snapshot.baseline_generation_no, 1);
}

#[test]
fn work_item_typed_crud_and_working_state_upsert() {
    let dir = TestDir::create("workspace-item");
    let store = WorkspaceKnowledgeStore::open(&dir.db("workspace.db")).expect("workspace.db opens");
    let item = store
        .create_work_item(&new_work_item("implement task 1"))
        .expect("create");
    assert_eq!(item.status, WorkItemStatus::Open);
    assert_eq!(
        store.get_work_item(item.uid).expect("get"),
        Some(item.clone())
    );

    let active = store
        .set_work_item_status(item.uid, WorkItemStatus::Active)
        .expect("activate");
    assert_eq!(
        store
            .list_work_items(WorkItemStatus::Active, 10)
            .expect("list"),
        vec![active]
    );

    let first = store
        .upsert_working_state(&working_state(item.uid, "schema"))
        .expect("state");
    let second = store
        .upsert_working_state(&working_state(item.uid, "tests"))
        .expect("state");
    assert_eq!(first.current_step.as_deref(), Some("schema"));
    assert_eq!(second.current_step.as_deref(), Some("tests"));
    assert_eq!(
        store.get_working_state(item.uid).expect("get"),
        Some(second)
    );

    let done = store
        .set_work_item_status(item.uid, WorkItemStatus::Completed)
        .expect("complete");
    assert!(done.closed_at.is_some());
    assert!(
        store
            .set_work_item_status(item.uid, WorkItemStatus::Active)
            .is_err()
    );
    assert!(matches!(
        store.upsert_working_state(&working_state(WorkItemId::generate(), "x")),
        Err(KnowledgeError::NotFound { .. })
    ));
}

#[test]
fn work_resource_references_resource_by_value_without_cross_db_fk() {
    let dir = TestDir::create("workspace-resource");
    let path = dir.db("workspace.db");
    let store = WorkspaceKnowledgeStore::open(&path).expect("workspace.db opens");
    let item = store.create_work_item(&new_work_item("g")).expect("create");
    let resource = ResourceId::generate();

    let recorded = store
        .record_work_resource(&WorkResource {
            work_item: item.uid,
            resource,
            role: WorkResourceRole::Target,
            locator_hint: Some("crates/engine/src/lib.rs".to_owned()),
            first_observed_revision: "r1".to_owned(),
            last_observed_revision: "r1".to_owned(),
        })
        .expect("no index.db needed");
    let advanced = store
        .record_work_resource(&WorkResource {
            last_observed_revision: "r4".to_owned(),
            first_observed_revision: "ignored".to_owned(),
            ..recorded.clone()
        })
        .expect("re-record");
    assert_eq!(advanced.first_observed_revision, "r1");
    assert_eq!(advanced.last_observed_revision, "r4");
    assert_eq!(
        store.list_work_resources(item.uid, 10).expect("list"),
        vec![advanced]
    );

    let connection = Connection::open(&path).expect("raw connection");
    let fk_tables: Vec<String> = connection
        .prepare("SELECT \"table\" FROM pragma_foreign_key_list('work_resource')")
        .expect("fk listing")
        .query_map([], |row| row.get(0))
        .expect("fk query")
        .collect::<Result<_, _>>()
        .expect("fk decode");
    assert_eq!(fk_tables, vec!["work_item".to_owned()]);
}

#[test]
fn work_result_and_handoff_typed_reads() {
    let dir = TestDir::create("workspace-result");
    let store = WorkspaceKnowledgeStore::open(&dir.db("workspace.db")).expect("workspace.db opens");
    let item = store.create_work_item(&new_work_item("g")).expect("create");

    let result = store
        .record_work_result(&WorkResult {
            work_item: item.uid,
            result_status: WorkResultStatus::Partial,
            result_summary: "schema only".to_owned(),
            commit_id: Some("abc123".to_owned()),
            change_set_fingerprint: None,
            verification_summary: Some("fmt ok".to_owned()),
            result_workspace_revision: "r9".to_owned(),
            result_generation_no: Some(9),
            created_at: String::new(),
        })
        .expect("result");
    assert_eq!(result.result_status, WorkResultStatus::Partial);
    assert_eq!(result.commit_id.as_deref(), Some("abc123"));
    assert_eq!(store.get_work_result(item.uid).expect("get"), Some(result));

    for summary in ["first", "second", "third"] {
        store
            .add_work_handoff(&WorkHandoff {
                work_item: item.uid,
                handoff_summary: summary.to_owned(),
                remaining_summary: Some("tests".to_owned()),
                blocker_summary: None,
                next_scope_hint: None,
                created_at: String::new(),
            })
            .expect("handoff");
    }
    let latest = store.list_work_handoffs(item.uid, 2).expect("handoffs");
    assert_eq!(
        latest
            .iter()
            .map(|h| h.handoff_summary.as_str())
            .collect::<Vec<_>>(),
        vec!["third", "second"]
    );
}

#[test]
fn work_notes_round_trip_and_proposal_stays_a_note() {
    let dir = TestDir::create("workspace-note");
    let path = dir.db("workspace.db");
    let store = WorkspaceKnowledgeStore::open(&path).expect("workspace.db opens");
    let item = store.create_work_item(&new_work_item("g")).expect("create");

    let observation = store
        .add_work_note(
            item.uid,
            &NewWorkNote {
                kind: WorkNoteKind::Observation,
                note_text: "4 routes found".to_owned(),
                provenance: Provenance::new(SourceKind::Observed)
                    .with_locator("parser")
                    .with_revision("gen-3"),
            },
        )
        .expect("observation");
    assert_eq!(observation.status, WorkNoteStatus::Open);
    assert_eq!(observation.promoted_item, None);
    assert_eq!(
        store.get_work_note(observation.uid).expect("get"),
        Some(observation.clone())
    );

    let proposal = store
        .add_work_note(
            item.uid,
            &NewWorkNote {
                kind: WorkNoteKind::Proposal,
                note_text: "split the Roslyn worker".to_owned(),
                provenance: Provenance::new(SourceKind::AgentReported),
            },
        )
        .expect("proposal");
    assert_eq!(proposal.kind, WorkNoteKind::Proposal);
    assert_eq!(
        store
            .list_work_notes(
                item.uid,
                Some(WorkNoteKind::Proposal),
                Some(WorkNoteStatus::Open),
                10
            )
            .expect("list"),
        vec![proposal.clone()]
    );
    assert_eq!(
        store
            .list_work_notes(item.uid, None, None, 10)
            .expect("list")
            .len(),
        2
    );

    // Task 1 has no path to PROMOTED; task 4 owns promotion.
    assert!(
        store
            .set_work_note_status(proposal.uid, WorkNoteStatus::Promoted)
            .is_err()
    );
    let discarded = store
        .set_work_note_status(proposal.uid, WorkNoteStatus::Discarded)
        .expect("discard");
    assert_eq!(discarded.status, WorkNoteStatus::Discarded);
    assert!(
        store
            .set_work_note_status(proposal.uid, WorkNoteStatus::Open)
            .is_err()
    );

    // workspace.db cannot hold Policy/Decision payload at all.
    let tables = table_names(&Connection::open(&path).expect("raw connection"));
    assert!(
        !tables
            .iter()
            .any(|t| t.contains("policy") || t.contains("decision"))
    );

    // A promoted note written by (future) task 4 decodes to a typed target.
    let target = DecisionId::generate();
    Connection::open(&path)
        .expect("raw connection")
        .execute(
            "UPDATE work_note SET status = 'PROMOTED', promoted_item_kind = 'DECISION', \
             promoted_item_uid = ?1 WHERE uid = ?2",
            params![
                target.to_bytes().to_vec(),
                observation.uid.to_bytes().to_vec()
            ],
        )
        .expect("raw promotion");
    assert_eq!(
        store
            .get_work_note(observation.uid)
            .expect("get")
            .and_then(|n| n.promoted_item),
        Some(PromotedItem::Decision(target))
    );
}

#[test]
fn workspace_project_state_round_trips() {
    let dir = TestDir::create("workspace-state");
    let store = WorkspaceKnowledgeStore::open(&dir.db("workspace.db")).expect("workspace.db opens");
    let created = store
        .upsert_workspace_project_state(&state(
            "migration_applied",
            KnowledgeScope::project(),
            TypedValue::Boolean(true),
        ))
        .expect("upsert");
    assert_eq!(
        store
            .get_workspace_project_state("migration_applied", &KnowledgeScope::project())
            .expect("get"),
        Some(created.clone())
    );
    assert_eq!(
        store
            .list_workspace_project_state(&KnowledgeScope::project(), 10)
            .expect("list"),
        vec![created]
    );
    assert!(
        store
            .upsert_workspace_project_state(&state(
                "x",
                KnowledgeScope::global(),
                TypedValue::Integer(1)
            ))
            .is_err()
    );
}

#[test]
fn state_identity_is_unique_for_null_and_non_null_scope_keys() {
    let dir = TestDir::create("state-identity");
    let workspace_path = dir.db("workspace.db");
    let project_path = dir.db("project.db");
    drop(WorkspaceKnowledgeStore::open(&workspace_path).expect("workspace.db"));
    drop(ProjectKnowledgeStore::open(&project_path).expect("project.db"));

    let raw_insert =
        |path: &Path, table: &str, uid: bool, scope_kind: &str, scope_key: Option<&str>| {
            let (uid_column, uid_value) = if uid {
                ("uid, ", "randomblob(16), ")
            } else {
                ("", "")
            };
            Connection::open(path).expect("raw").execute(
            &format!(
                "INSERT INTO {table} ({uid_column}state_key, scope_kind, scope_key, value_type, \
                 value_json, status, source_kind, updated_at) \
                 VALUES ({uid_value}'k', ?1, ?2, 'INTEGER', '1', 'CURRENT', 'OBSERVED', '0')"
            ),
            params![scope_kind, scope_key],
        )
        };

    for (path, table) in [
        (&workspace_path, "workspace_project_state"),
        (&project_path, "project_state"),
    ] {
        raw_insert(path, table, true, "PROJECT", None).expect("first NULL-key row");
        assert!(
            raw_insert(path, table, true, "PROJECT", None).is_err(),
            "{table}: a second NULL-key row with the same key/kind must be rejected"
        );
        raw_insert(path, table, true, "MODULE", Some("a")).expect("keyed row");
        raw_insert(path, table, true, "MODULE", Some("b")).expect("other key");
        assert!(raw_insert(path, table, true, "MODULE", Some("a")).is_err());
    }
}

#[test]
fn two_workspaces_keep_working_state_isolated() {
    let dir = TestDir::create("two-workspaces");
    let a = WorkspaceKnowledgeStore::open(&dir.db("a-workspace.db")).expect("A");
    let b = WorkspaceKnowledgeStore::open(&dir.db("b-workspace.db")).expect("B");

    let item = a
        .create_work_item(&new_work_item("A only"))
        .expect("create");
    a.upsert_working_state(&working_state(item.uid, "A step"))
        .expect("state");

    assert_eq!(b.get_work_item(item.uid).expect("get"), None);
    assert_eq!(b.get_working_state(item.uid).expect("get"), None);
    assert!(
        b.list_work_items(WorkItemStatus::Open, 10)
            .expect("list")
            .is_empty()
    );
    assert!(matches!(
        b.add_work_note(
            item.uid,
            &NewWorkNote {
                kind: WorkNoteKind::Observation,
                note_text: "x".to_owned(),
                provenance: Provenance::new(SourceKind::Observed),
            }
        ),
        Err(KnowledgeError::NotFound { .. })
    ));
}

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
fn secondary_worktree_shares_project_knowledge_but_not_workspace_knowledge() {
    let home = TestDir::create("worktree-home");
    let main = TestDir::create("worktree-main");
    let secondary = TestDir::create("worktree-secondary");
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
    let main_paths = WorkspacePaths::from_root(&main_init.workspace_root);
    let secondary_paths = WorkspacePaths::from_root(&secondary_init.workspace_root);

    let registry = GlobalRegistry::open(&global.global_db).expect("registry");
    let from_main = ProjectKnowledgeStore::open_project_home(&registry, main_init.project_id)
        .expect("main opens project-home");
    let from_secondary =
        ProjectKnowledgeStore::open_project_home(&registry, secondary_init.project_id)
            .expect("secondary opens the same project-home");

    let policy = from_main
        .insert_policy(&new_policy(KnowledgeScope::project(), "shared"))
        .expect("policy");
    let decision = from_main
        .insert_decision(&new_decision("baseline"))
        .expect("decision");
    let blueprint = from_main
        .insert_blueprint(&new_blueprint(
            KnowledgeScope::project(),
            BlueprintStatus::Active,
        ))
        .expect("blueprint");
    from_main
        .upsert_project_state(&state(
            "milestone",
            KnowledgeScope::project(),
            TypedValue::Text("I5".into()),
        ))
        .expect("state");
    let worktree_policy = from_secondary
        .insert_policy(&new_policy(
            KnowledgeScope::workspace(secondary_init.workspace_id),
            "wt",
        ))
        .expect("worktree-scoped policy lives in project.db");

    assert_eq!(
        from_secondary.get_policy(policy.uid).expect("get"),
        Some(policy)
    );
    assert!(
        from_secondary
            .get_decision(decision.uid)
            .expect("get")
            .is_some()
    );
    assert!(
        from_secondary
            .get_blueprint(blueprint.uid)
            .expect("get")
            .is_some()
    );
    assert!(
        from_secondary
            .get_project_state("milestone", &KnowledgeScope::project())
            .expect("get")
            .is_some()
    );
    assert_eq!(
        from_main.get_policy(worktree_policy.uid).expect("get"),
        Some(worktree_policy)
    );
    assert!(!secondary_paths.project_db.exists(), "no second project.db");

    let main_ws = WorkspaceKnowledgeStore::open(&main_paths.workspace_db).expect("main ws");
    let secondary_ws =
        WorkspaceKnowledgeStore::open(&secondary_paths.workspace_db).expect("sec ws");
    let item = secondary_ws
        .create_work_item(&new_work_item("feature work"))
        .expect("item");
    secondary_ws
        .upsert_working_state(&working_state(item.uid, "wt step"))
        .expect("state");
    secondary_ws
        .add_work_note(
            item.uid,
            &NewWorkNote {
                kind: WorkNoteKind::OpenQuestion,
                note_text: "q".to_owned(),
                provenance: Provenance::new(SourceKind::AgentReported),
            },
        )
        .expect("note");
    secondary_ws
        .upsert_workspace_project_state(&state(
            "migration",
            KnowledgeScope::project(),
            TypedValue::Boolean(true),
        ))
        .expect("workspace state");

    assert_eq!(main_ws.get_work_item(item.uid).expect("get"), None);
    assert_eq!(main_ws.get_working_state(item.uid).expect("get"), None);
    assert_eq!(
        main_ws
            .get_workspace_project_state("migration", &KnowledgeScope::project())
            .expect("get"),
        None
    );
}

#[test]
fn missing_project_home_is_reported_not_created() {
    let home = TestDir::create("missing-home");
    let workspace = TestDir::create("missing-workspace");
    let global = GlobalPaths::from_home(home.path());
    let outcome = init_workspace(workspace.path(), &global).expect("init");
    let paths = WorkspacePaths::from_root(&outcome.workspace_root);
    fs::remove_file(&paths.project_db).expect("remove project.db");

    let registry = GlobalRegistry::open(&global.global_db).expect("registry");
    assert!(matches!(
        ProjectKnowledgeStore::open_project_home(&registry, outcome.project_id),
        Err(KnowledgeError::ProjectHomeMissing { .. })
    ));
    assert!(!paths.project_db.exists());
}

// ============================================================= lifecycle

#[test]
fn durable_knowledge_survives_index_db_removal_and_recreation() {
    let home = TestDir::create("rebuild-home");
    let workspace = TestDir::create("rebuild-workspace");
    let global = GlobalPaths::from_home(home.path());
    let outcome = init_workspace(workspace.path(), &global).expect("init");
    let paths = WorkspacePaths::from_root(&outcome.workspace_root);
    assert!(paths.index_db.is_file());

    let global_store = GlobalKnowledgeStore::open(&global.global_db).expect("global");
    let user_policy = global_store
        .insert_user_policy(&new_policy(KnowledgeScope::global(), "g"))
        .expect("user policy");
    let preference = global_store
        .insert_user_preference(&NewUserPreference {
            scope: KnowledgeScope::global(),
            preference_key: "language".to_owned(),
            value: TypedValue::Text("ko".to_owned()),
            provenance: user_explicit(),
        })
        .expect("preference");
    let global_blueprint = global_store
        .insert_blueprint(&new_blueprint(
            KnowledgeScope::global(),
            BlueprintStatus::Active,
        ))
        .expect("global blueprint");

    let registry = GlobalRegistry::open(&global.global_db).expect("registry");
    let project =
        ProjectKnowledgeStore::open_project_home(&registry, outcome.project_id).expect("project");
    let policy = project
        .insert_policy(&new_policy(KnowledgeScope::project(), "p"))
        .expect("policy");
    let decision = project
        .insert_decision(&new_decision("t"))
        .expect("decision");
    let blueprint = project
        .insert_blueprint(&new_blueprint(
            KnowledgeScope::project(),
            BlueprintStatus::Draft,
        ))
        .expect("blueprint");
    project
        .upsert_project_state(&state(
            "milestone",
            KnowledgeScope::project(),
            TypedValue::Text("I5".into()),
        ))
        .expect("state");

    let ws = WorkspaceKnowledgeStore::open(&paths.workspace_db).expect("workspace");
    let item = ws.create_work_item(&new_work_item("g")).expect("item");
    ws.upsert_working_state(&working_state(item.uid, "s"))
        .expect("working state");
    let note = ws
        .add_work_note(
            item.uid,
            &NewWorkNote {
                kind: WorkNoteKind::Observation,
                note_text: "n".to_owned(),
                provenance: Provenance::new(SourceKind::Observed),
            },
        )
        .expect("note");
    ws.upsert_workspace_project_state(&state(
        "local",
        KnowledgeScope::project(),
        TypedValue::Integer(1),
    ))
    .expect("workspace state");
    drop((global_store, registry, project, ws));

    // Remove the rebuildable DB (and its WAL sidecars), then let the
    // existing repeated-init path recreate it.
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", paths.index_db.display()));
    }
    assert!(!paths.index_db.exists());
    init_workspace(workspace.path(), &global).expect("re-init recreates index.db");
    assert!(paths.index_db.is_file());

    let global_store = GlobalKnowledgeStore::open(&global.global_db).expect("global");
    assert!(
        global_store
            .get_user_policy(user_policy.uid)
            .expect("get")
            .is_some()
    );
    assert!(
        global_store
            .get_user_preference(preference.uid)
            .expect("get")
            .is_some()
    );
    assert!(
        global_store
            .get_blueprint(global_blueprint.uid)
            .expect("get")
            .is_some()
    );
    let registry = GlobalRegistry::open(&global.global_db).expect("registry");
    let project =
        ProjectKnowledgeStore::open_project_home(&registry, outcome.project_id).expect("project");
    assert!(project.get_policy(policy.uid).expect("get").is_some());
    assert!(project.get_decision(decision.uid).expect("get").is_some());
    assert!(project.get_blueprint(blueprint.uid).expect("get").is_some());
    assert!(
        project
            .get_project_state("milestone", &KnowledgeScope::project())
            .expect("get")
            .is_some()
    );
    let ws = WorkspaceKnowledgeStore::open(&paths.workspace_db).expect("workspace");
    assert!(ws.get_work_item(item.uid).expect("get").is_some());
    assert!(ws.get_working_state(item.uid).expect("get").is_some());
    assert!(ws.get_work_note(note.uid).expect("get").is_some());
    assert!(
        ws.get_workspace_project_state("local", &KnowledgeScope::project())
            .expect("get")
            .is_some()
    );
}

#[test]
fn migrations_are_idempotent_on_reopen() {
    let dir = TestDir::create("idempotent");
    for (name, kind, expected) in [
        ("global.db", DbKind::Global, 4u32),
        ("project.db", DbKind::Project, 3),
        ("workspace.db", DbKind::Workspace, 3),
    ] {
        let path = dir.db(name);
        for _ in 0..3 {
            let opened = match kind {
                DbKind::Global => schema::global::open(&path),
                DbKind::Project => schema::project::open(&path),
                _ => schema::workspace::open(&path),
            }
            .expect("open");
            assert_eq!(opened.schema_version, expected, "{name}");
            let ledger: u32 = opened
                .connection
                .query_row("SELECT COUNT(*) FROM schema_migration", [], |row| {
                    row.get(0)
                })
                .expect("ledger");
            assert_eq!(ledger, expected, "{name} ledger must not grow on reopen");
        }
    }
}

#[test]
fn wrong_db_kind_is_rejected() {
    let dir = TestDir::create("wrong-kind");
    let project_path = dir.db("project.db");
    drop(ProjectKnowledgeStore::open(&project_path).expect("project.db"));

    assert!(matches!(
        WorkspaceKnowledgeStore::open(&project_path),
        Err(KnowledgeError::Open(db::DbOpenError::KindMismatch { .. }))
    ));
    assert!(matches!(
        GlobalKnowledgeStore::open(&project_path),
        Err(KnowledgeError::Open(db::DbOpenError::KindMismatch { .. }))
    ));
}

#[test]
fn no_generic_knowledge_store_exists() {
    let dir = TestDir::create("no-generic");
    drop(GlobalKnowledgeStore::open(&dir.db("global.db")).expect("global"));
    drop(ProjectKnowledgeStore::open(&dir.db("project.db")).expect("project"));
    drop(WorkspaceKnowledgeStore::open(&dir.db("workspace.db")).expect("workspace"));

    let tables = |name: &str| table_names(&Connection::open(dir.db(name)).expect("raw"));
    assert_eq!(
        tables("global.db"),
        [
            "blueprint",
            "db_meta",
            "project_git_lineage",
            "project_registry",
            "schema_migration",
            "user_policy",
            "user_policy_link",
            "user_preference",
            "workspace_registry",
        ]
    );
    assert_eq!(
        tables("project.db"),
        [
            "blueprint",
            "blueprint_application",
            "db_meta",
            "decision",
            "decision_link",
            "policy",
            "policy_link",
            "project_state",
            "schema_migration",
        ]
    );
    assert_eq!(
        tables("workspace.db"),
        [
            "db_meta",
            "schema_migration",
            "work_handoff",
            "work_item",
            "work_note",
            "work_resource",
            "work_result",
            "working_state",
            "workspace_project_state",
            "workspace_state",
        ]
    );
}

// ============================================================ invariants

#[test]
fn scope_round_trips_and_rejects_malformed_pairs() {
    let workspace_id = WorkspaceId::generate();
    let valid = [
        KnowledgeScope::global(),
        KnowledgeScope::project(),
        KnowledgeScope::workspace(workspace_id),
        KnowledgeScope::keyed(ScopeKind::Package, "brainprint-engine").expect("package"),
        KnowledgeScope::keyed(ScopeKind::Module, "knowledge").expect("module"),
        KnowledgeScope::keyed(ScopeKind::Directory, "crates/engine").expect("directory"),
        KnowledgeScope::keyed(ScopeKind::Resource, ResourceId::generate().to_string())
            .expect("resource"),
        KnowledgeScope::keyed(ScopeKind::Domain, "privacy").expect("domain"),
        KnowledgeScope::keyed(ScopeKind::Task, "I5-1").expect("task"),
    ];
    for scope in &valid {
        let decoded =
            KnowledgeScope::from_parts(scope.kind().as_str(), scope.key().map(str::to_owned))
                .expect("round trip");
        assert_eq!(&decoded, scope);
    }

    assert!(KnowledgeScope::keyed(ScopeKind::Global, "x").is_err());
    assert!(KnowledgeScope::keyed(ScopeKind::Project, "x").is_err());
    assert!(KnowledgeScope::keyed(ScopeKind::Module, "").is_err());
    assert!(KnowledgeScope::keyed(ScopeKind::Workspace, "not-a-workspace-id").is_err());
    assert!(KnowledgeScope::from_parts("MODULE", None).is_err());
    assert!(matches!(
        KnowledgeScope::from_parts("TEAM", None),
        Err(KnowledgeError::UnknownValue {
            vocabulary: "ScopeKind",
            ..
        })
    ));
}

#[test]
fn stable_uids_round_trip_through_storage_form() {
    let policy = PolicyId::generate();
    assert_eq!(
        uid_from_blob::<PolicyId>(&blob(policy), "t").expect("decode"),
        policy
    );
    let note = WorkNoteId::generate();
    assert_eq!(
        uid_from_blob::<WorkNoteId>(&blob(note), "t").expect("decode"),
        note
    );
    assert!(matches!(
        uid_from_blob::<PolicyId>(&[1, 2, 3], "t"),
        Err(KnowledgeError::CorruptUid { .. })
    ));
}

#[test]
fn unknown_vocabulary_strings_are_rejected_not_coerced() {
    assert!(is_unknown(
        &PolicyStatus::parse("PROPOSED").unwrap_err().into(),
        "PolicyStatus"
    ));
    assert!(is_unknown(
        &DecisionStatus::parse("active").unwrap_err().into(),
        "DecisionStatus"
    ));
    assert!(is_unknown(
        &BlueprintOwnerKind::parse("WORKSPACE").unwrap_err().into(),
        "BlueprintOwnerKind"
    ));
    assert!(is_unknown(
        &WorkNoteKind::parse("MEMORY").unwrap_err().into(),
        "WorkNoteKind"
    ));
    assert!(is_unknown(
        &SourceKind::parse("MANUAL").unwrap_err().into(),
        "SourceKind"
    ));
    assert!(TypedValue::from_parts("INTEGER", "\"12\"").is_err());
    assert!(TypedValue::from_parts("TEXT", "not json").is_err());
    assert!(TypedValue::from_parts("BLOB", "1").is_err());
    assert_eq!(
        TypedValue::from_parts("INTEGER", "12").expect("decode"),
        TypedValue::Integer(12)
    );
}

// ======================================================= EXPLAIN QUERY PLAN

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

/// Task 1 SQL access plan (#20): every hot query's plan, asserting the
/// intended index (or UNIQUE autoindex) is used and no hot path scans a
/// growing knowledge table. Run with `--nocapture` to print the plans.
#[test]
fn task1_access_paths_use_their_intended_indexes() {
    let dir = TestDir::create("eqp");
    let (g, p, w) = (
        dir.db("global.db"),
        dir.db("project.db"),
        dir.db("workspace.db"),
    );
    drop(GlobalKnowledgeStore::open(&g).expect("global"));
    drop(ProjectKnowledgeStore::open(&p).expect("project"));
    drop(WorkspaceKnowledgeStore::open(&w).expect("workspace"));
    let (g, p, w) = (
        Connection::open(g).expect("g"),
        Connection::open(p).expect("p"),
        Connection::open(w).expect("w"),
    );
    let uid = vec![0u8; 16];
    let scoped =
        "scope_kind = 'PROJECT' AND scope_key IS NULL AND status = 'ACTIVE' ORDER BY id LIMIT 10";
    let state = "scope_kind = 'PROJECT' AND ifnull(scope_key, '') = '' AND state_key = 'k'";

    let cases: Vec<(&str, &Connection, String, &str)> = vec![
        ("P1 policy by uid", &p, "SELECT * FROM policy WHERE uid = ?1".into(), "sqlite_autoindex_policy_1"),
        ("P2 policies by scope+status", &p, format!("SELECT * FROM policy WHERE {scoped}"), "idx_policy_scope_status"),
        ("P3 decision by uid", &p, "SELECT * FROM decision WHERE uid = ?1".into(), "sqlite_autoindex_decision_1"),
        ("P4 decisions by topic+status", &p, "SELECT * FROM decision WHERE topic = 't' AND status = 'ACTIVE' ORDER BY id LIMIT 10".into(), "idx_decision_topic_status"),
        ("P5 policy lineage out", &p, "SELECT other.uid FROM policy this JOIN policy_link l ON l.policy_id = this.id JOIN policy other ON other.id = l.related_policy_id WHERE this.uid = ?1 ORDER BY other.id, l.link_kind".into(), "sqlite_autoindex_policy_link_1"),
        ("P6 policy lineage in", &p, "SELECT other.uid FROM policy this JOIN policy_link l ON l.related_policy_id = this.id JOIN policy other ON other.id = l.policy_id WHERE this.uid = ?1 ORDER BY other.id, l.link_kind".into(), "idx_policy_link_related"),
        ("P7 decision lineage in", &p, "SELECT other.uid FROM decision this JOIN decision_link l ON l.related_decision_id = this.id JOIN decision other ON other.id = l.decision_id WHERE this.uid = ?1 ORDER BY other.id, l.link_kind".into(), "idx_decision_link_related"),
        ("B1 project blueprint by uid", &p, "SELECT * FROM blueprint WHERE uid = ?1".into(), "sqlite_autoindex_blueprint_1"),
        ("B2 project blueprints by scope+status", &p, format!("SELECT * FROM blueprint WHERE {scoped}"), "idx_blueprint_scope_status"),
        ("B3 applications by scope+status", &p, format!("SELECT * FROM blueprint_application WHERE {scoped}"), "idx_blueprint_application_scope_status"),
        ("S1 project state by key+scope", &p, format!("SELECT * FROM project_state WHERE {state}"), "idx_project_state_identity"),
        ("S2 project state by scope", &p, "SELECT * FROM project_state WHERE scope_kind = 'PROJECT' AND ifnull(scope_key, '') = '' ORDER BY state_key LIMIT 10".into(), "idx_project_state_identity"),
        ("G1 user policy by uid", &g, "SELECT * FROM user_policy WHERE uid = ?1".into(), "sqlite_autoindex_user_policy_1"),
        ("G2 user policies by scope+status", &g, format!("SELECT * FROM user_policy WHERE {scoped}"), "idx_user_policy_scope_status"),
        ("G3 user policy lineage in", &g, "SELECT other.uid FROM user_policy this JOIN user_policy_link l ON l.related_user_policy_id = this.id JOIN user_policy other ON other.id = l.user_policy_id WHERE this.uid = ?1".into(), "idx_user_policy_link_related"),
        ("G4 preference by scope+key+status", &g, "SELECT * FROM user_preference WHERE scope_kind = 'GLOBAL' AND scope_key IS NULL AND preference_key = 'k' AND status = 'ACTIVE' ORDER BY id LIMIT 5".into(), "idx_user_preference_scope_key_status"),
        ("G5 preferences by scope+status", &g, "SELECT * FROM user_preference WHERE scope_kind = 'GLOBAL' AND scope_key IS NULL AND status = 'ACTIVE' ORDER BY preference_key, id LIMIT 10".into(), "idx_user_preference_scope_key_status"),
        ("G6 global blueprints by scope+status", &g, "SELECT * FROM blueprint WHERE scope_kind = 'GLOBAL' AND scope_key IS NULL AND status = 'ACTIVE' ORDER BY id LIMIT 10".into(), "idx_blueprint_scope_status"),
        ("W1 work item by uid", &w, "SELECT * FROM work_item WHERE uid = ?1".into(), "sqlite_autoindex_work_item_1"),
        ("W2 work items by status", &w, "SELECT * FROM work_item WHERE status = 'OPEN' ORDER BY id LIMIT 10".into(), "idx_work_item_status"),
        ("W3 working state by item", &w, "SELECT * FROM work_item w JOIN working_state s ON s.work_item_id = w.id WHERE w.uid = ?1".into(), "sqlite_autoindex_working_state_1"),
        ("W4 work resources by item", &w, "SELECT * FROM work_resource r JOIN work_item w ON w.id = r.work_item_id WHERE r.work_item_id = 1 ORDER BY r.id LIMIT 10".into(), "idx_work_resource_work_item"),
        ("W5 work result by item", &w, "SELECT * FROM work_item w JOIN work_result r ON r.work_item_id = w.id WHERE w.uid = ?1".into(), "sqlite_autoindex_work_result_1"),
        ("W6 handoffs by item", &w, "SELECT * FROM work_handoff h JOIN work_item w ON w.id = h.work_item_id WHERE h.work_item_id = 1 ORDER BY h.id DESC LIMIT 2".into(), "idx_work_handoff_work_item"),
        ("W7 work note by uid", &w, "SELECT * FROM work_note n JOIN work_item w ON w.id = n.work_item_id WHERE n.uid = ?1".into(), "sqlite_autoindex_work_note_1"),
        ("W8 work notes by item+kind+status", &w, "SELECT * FROM work_note n JOIN work_item w ON w.id = n.work_item_id WHERE n.work_item_id = 1 AND n.kind = 'PROPOSAL' AND n.status = 'OPEN' ORDER BY n.id LIMIT 10".into(), "idx_work_note_item_kind_status"),
        ("W9 workspace state by key+scope", &w, format!("SELECT * FROM workspace_project_state WHERE {state}"), "idx_workspace_project_state_identity"),
    ];

    for (label, connection, sql, index) in cases {
        let needs_uid = sql.contains("?1");
        let found = if needs_uid {
            plan(connection, &sql, params![uid])
        } else {
            plan(connection, &sql, [])
        };
        println!("{label}: {found}");
        assert!(
            found.contains(index),
            "{label}: expected {index}, got {found}"
        );
        // No hot path may scan a knowledge table.
        assert!(
            !found.split(" | ").any(|step| step.starts_with("SCAN ")),
            "{label}: unexpected full scan in {found}"
        );
    }
}
