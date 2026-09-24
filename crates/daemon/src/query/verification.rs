//! #24 "Task 11 verification correction" completion pass.
//!
//! Additive only -- does not modify the locked #24 contract, the existing
//! `i5_task11_query_acceptance.rs` coverage, or any Task 10 semantics.
//! Closes exactly the gaps #24 names:
//!
//! - a populated fixture (real files, indexed through the real
//!   `BaselineScan` pipeline; real Policy/Decision/WorkItem/Handoff
//!   through the real `ProjectKnowledgeStore`/`WorkRuntime`), run once
//!   through `CoreQuerySurface` directly and once through the daemon wire,
//!   for all thirteen operation/mode combinations (acceptance 13-19);
//! - Task-11-level (through the real daemon protocol) Workspace locator
//!   ambiguity, binding mismatch and two-worktree isolation coverage
//!   (acceptance 8-12, 39-41);
//! - real Core-stats-based source-read/byte and backend-activity
//!   measurement (acceptance 20-24), not code inspection.
//!
//! Parity is checked by converting the *direct* `CoreQuerySurface` answer
//! through the same `convert_out` functions the daemon itself calls, then
//! comparing that against the real wire `QueryResultWire` the daemon
//! actually returned over a real socket. This isolates exactly the risk
//! #24 flags: whether daemon dispatch (workspace resolution, worker
//! routing, budget/retention handling) reaches the same Core answer as a
//! direct call -- not a second, hand-written re-derivation of the 1:1
//! field mirror the type system already guarantees. Nothing here re-sorts
//! or reinterprets a result to make a comparison pass.

use std::{
    env, fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use brainprint_core::{
    DecisionId, PROTOCOL_VERSION, PolicyId, WorkItemId, WorkspaceId,
    protocol::{
        self, ClientConnection, HandshakeRequest, InitRequest, InitResponse, Request, Response,
        query::*,
    },
};
use brainprint_engine::{
    boundary::{GroupingSpec, ResourceScope},
    config::WorkspaceConfig,
    graph::GraphStore,
    impact::ImpactIntent,
    knowledge::{
        DirtyObservation, NewDecision, NewPolicy, NewWorkItem, PriorityClass,
        ProjectKnowledgeStore, ProtectionClass, Provenance, SourceKind, StartObservation,
        WorkHandoff, WorkItemSourceKind, WorkItemStatus, WorkRuntime,
    },
    paths::{GlobalPaths, WorkspacePaths},
    projection::{
        ChangeKind, ProjectionKnowledgeRefs, ProjectionTarget, ResourceTarget,
        planner::{ContextRetention, DeliveryBudget, DeliveryLedger, LedgerLimits},
    },
    query_surface::{
        ContextPurpose, ContextRequest, CoreQuerySurface, DeliveryOptions, FindQuery, FindRequest,
        ImpactRequest, InspectRequest, KnowledgeQuery, KnowledgeRequest, LineageTarget,
        OwnedTextPattern, QueryContext, RelationDirection, RelationsRequest,
    },
    registry::GlobalRegistry,
    scan::BaselineScan,
    search::SearchBudget,
};

use super::convert_out;
use crate::{runtime_paths, server::Server};

// --------------------------------------------------------------- harness

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestHome(PathBuf);

impl TestHome {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-i5-task11-verify-{label}-{}-{sequence}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("test dir should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn global_paths(&self) -> GlobalPaths {
        GlobalPaths::from_home(&self.0)
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn send(connection: &mut ClientConnection, request: Request) -> Response {
    protocol::framing::write_message(connection, &request)
        .await
        .expect("write should succeed");
    protocol::framing::read_message(connection)
        .await
        .expect("read should succeed")
}

async fn start_server(
    global_paths: &GlobalPaths,
) -> (tokio::task::JoinHandle<()>, runtime_paths::RuntimeEndpoint) {
    let mut server = Server::bind(global_paths)
        .await
        .expect("bind should succeed");
    let endpoint = runtime_paths::resolve(global_paths);
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (handle, endpoint)
}

async fn connect(endpoint: &runtime_paths::RuntimeEndpoint) -> ClientConnection {
    #[cfg(unix)]
    let connection = ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    let connection = ClientConnection::connect(&endpoint.pipe_name).await;
    connection.expect("connect should succeed")
}

async fn handshake(connection: &mut ClientConnection, protocol_version: u32) -> Response {
    send(
        connection,
        Request::Handshake(HandshakeRequest {
            protocol_version,
            client_kind: "test-client".to_owned(),
        }),
    )
    .await
}

async fn init_workspace(connection: &mut ClientConnection, path: &Path) -> WorkspaceId {
    let response = send(
        connection,
        Request::Init(InitRequest {
            path: path.to_string_lossy().into_owned(),
        }),
    )
    .await;
    let Response::Init(InitResponse { workspace_id, .. }) = response else {
        panic!("expected Init response, got {response:?}")
    };
    workspace_id
        .parse()
        .expect("workspace_id should be a valid UUID")
}

async fn ready_connection(endpoint: &runtime_paths::RuntimeEndpoint) -> ClientConnection {
    let mut connection = connect(endpoint).await;
    handshake(&mut connection, PROTOCOL_VERSION).await;
    connection
}

async fn query(
    connection: &mut ClientConnection,
    workspace_id: WorkspaceId,
    operation: QueryOperationWire,
) -> QueryResponse {
    let request = Request::Query(QueryRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        workspace: WorkspaceSelectorWire::Id { workspace_id },
        correlation: None,
        operation,
    });
    let Response::Query(response) = send(connection, request).await else {
        panic!("expected a Query response")
    };
    response
}

fn ok_result(response: QueryResponse) -> QueryResultWire {
    match response.outcome {
        QueryOutcomeWire::Ok(result) => result,
        QueryOutcomeWire::Err(error) => panic!("expected Ok, got {error:?}"),
    }
}

fn compact_wire_delivery() -> DeliveryWire {
    DeliveryWire {
        budget: DeliveryBudgetWire {
            max_items: NonZeroUsize::new(64),
            max_bytes: NonZeroUsize::new(64 * 1024),
        },
        continuation: None,
        retention: RetentionWire::Disabled,
    }
}

fn compact_direct_delivery() -> DeliveryOptions<'static> {
    DeliveryOptions {
        budget: DeliveryBudget::new(Some(64), Some(64 * 1024), None).expect("budget"),
        continuation: None,
        retention: ContextRetention::ReuseDisabled,
        tokens: None,
    }
}

fn fresh_ledger() -> DeliveryLedger {
    DeliveryLedger::new(LedgerLimits::new(16, 1024).expect("16/1024 are non-zero ledger limits"))
}

// ------------------------------------------------------------- fixture

const SHARED_TS: &str = "export function helper(): number {\n  return 42;\n}\n";
const APP_TS: &str = "import { helper } from \"./shared\";\n\nexport function run(): number {\n  return helper();\n}\n";

/// A real, populated Workspace: two TypeScript files with a real
/// import + call relation (indexed through the real `BaselineScan`
/// pipeline, no synthetic graph rows), one project Policy, one project
/// Decision, and one started WorkItem with one Handoff (through the real
/// `ProjectKnowledgeStore`/`WorkRuntime`).
struct Populated {
    _home: TestHome,
    _workspace: TestHome,
    global: GlobalPaths,
    workspace_id: WorkspaceId,
    paths: WorkspacePaths,
    work_item: WorkItemId,
    policy: PolicyId,
    decision: DecisionId,
}

async fn populated_fixture(
    label: &str,
) -> (
    Populated,
    ClientConnection,
    tokio::task::JoinHandle<()>,
    runtime_paths::RuntimeEndpoint,
) {
    let home = TestHome::create(&format!("{label}-home"));
    let global_paths = home.global_paths();
    let (server, endpoint) = start_server(&global_paths).await;

    let workspace = TestHome::create(&format!("{label}-ws"));
    fs::create_dir_all(workspace.path().join("src")).expect("src dir");
    fs::write(workspace.path().join("src/shared.ts"), SHARED_TS).expect("shared.ts");
    fs::write(workspace.path().join("src/app.ts"), APP_TS).expect("app.ts");

    let mut connection = ready_connection(&endpoint).await;
    let workspace_id = init_workspace(&mut connection, workspace.path()).await;
    let paths = WorkspacePaths::from_root(workspace.path());

    BaselineScan::open(&paths.index_db)
        .expect("index.db")
        .run_initial_scan(workspace.path(), &WorkspaceConfig::default(), "rev-1")
        .expect("baseline scan");

    let project = ProjectKnowledgeStore::open(&paths.project_db).expect("project.db");
    let provenance = || Provenance::new(SourceKind::UserExplicit);
    let policy = project
        .insert_policy(&NewPolicy {
            scope: brainprint_engine::knowledge::KnowledgeScope::project(),
            policy_key: Some("task11-verify-policy".to_owned()),
            title: "keep helper() pure".to_owned(),
            rule_text: "helper() must stay side-effect-free".to_owned(),
            structured_rule: None,
            protection_class: ProtectionClass::Normal,
            priority_class: PriorityClass::Default,
            provenance: provenance(),
        })
        .expect("insert policy")
        .uid;
    let decision = project
        .insert_decision(&NewDecision {
            scope: brainprint_engine::knowledge::KnowledgeScope::project(),
            topic: "shared-helper-design".to_owned(),
            chosen_summary: "one exported helper() in shared.ts".to_owned(),
            rationale: "simplest shape that exercises a real cross-file relation".to_owned(),
            provenance: provenance(),
        })
        .expect("insert decision")
        .uid;

    let work = WorkRuntime::open(workspace_id, &paths.workspace_db, &paths.index_db)
        .expect("work runtime");
    let created = work
        .create(&NewWorkItem {
            source_kind: WorkItemSourceKind::Issue,
            source_ref: Some("#24".to_owned()),
            title: None,
            goal: "verify Task 11 parity".to_owned(),
        })
        .expect("create work item");
    work.start(
        created.uid,
        &StartObservation {
            head: None,
            dirty: DirtyObservation::Unknown,
            preexisting_dirty: Vec::new(),
            owner_agent: Some("task11-verification".to_owned()),
        },
    )
    .expect("start work item");
    work.add_handoff(&WorkHandoff {
        work_item: created.uid,
        handoff_summary: "populated fixture ready".to_owned(),
        remaining_summary: None,
        blocker_summary: None,
        next_scope_hint: None,
        created_at: String::new(),
    })
    .expect("add handoff");

    let fixture = Populated {
        _home: home,
        _workspace: workspace,
        global: global_paths,
        workspace_id,
        paths,
        work_item: created.uid,
        policy,
        decision,
    };
    (fixture, connection, server, endpoint)
}

fn direct_surface(fixture: &Populated) -> CoreQuerySurface {
    CoreQuerySurface::open(&fixture.global.global_db, fixture.workspace_id)
        .expect("CoreQuerySurface::open should succeed for the populated Workspace")
}

fn direct_context(fixture: &Populated) -> QueryContext {
    QueryContext {
        workspace: fixture.workspace_id,
        correlation: None,
    }
}

fn app_ts_target() -> ProjectionTarget {
    ProjectionTarget::Resource(ResourceTarget::Path("src/app.ts".to_owned()))
}

fn app_ts_target_wire() -> ProjectionTargetWire {
    ProjectionTargetWire::Resource(ResourceTargetWire::Path("src/app.ts".to_owned()))
}

fn shared_ts_target() -> ProjectionTarget {
    ProjectionTarget::Resource(ResourceTarget::Path("src/shared.ts".to_owned()))
}

fn shared_ts_target_wire() -> ProjectionTargetWire {
    ProjectionTargetWire::Resource(ResourceTargetWire::Path("src/shared.ts".to_owned()))
}

// ------------------------------------------------------- operation parity
//
// Acceptance 13-19: one fixture, once through Task 10 directly, once
// through the daemon Query protocol, `convert_out`-mirrored and compared
// for exact equality.

#[tokio::test]
async fn parity_find_target_resolved() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("find-target").await;
    let surface = direct_surface(&fixture);
    let mut ledger = fresh_ledger();
    let direct = surface
        .find(
            FindRequest {
                context: direct_context(&fixture),
                query: FindQuery::Target {
                    target: app_ts_target(),
                    delivery: compact_direct_delivery(),
                },
            },
            &mut ledger,
        )
        .expect("direct find target");
    let expected = convert_out::find_result_wire(direct).0;

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Find(FindQueryWire::Target {
            target: app_ts_target_wire(),
            delivery: compact_wire_delivery(),
        }),
    )
    .await;
    let QueryResultWire::Find(actual) = ok_result(response) else {
        panic!("expected Find result")
    };
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn parity_find_files() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("find-files").await;
    let surface = direct_surface(&fixture);
    let mut ledger = fresh_ledger();
    let direct = surface
        .find(
            FindRequest {
                context: direct_context(&fixture),
                query: FindQuery::Files {
                    directory: None,
                    recursive: true,
                    path_prefix: None,
                    role: None,
                    language: None,
                    kind: None,
                    limit: NonZeroUsize::new(50).unwrap(),
                },
            },
            &mut ledger,
        )
        .expect("direct find files");
    let expected = convert_out::find_result_wire(direct).0;

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Find(FindQueryWire::Files {
            directory: None,
            recursive: true,
            path_prefix: None,
            role: None,
            language: None,
            kind: None,
            limit: NonZeroUsize::new(50).unwrap(),
        }),
    )
    .await;
    let QueryResultWire::Find(actual) = ok_result(response) else {
        panic!("expected Find result")
    };
    assert_eq!(expected, actual);
    let FindResultWire::Files(listing) = &actual else {
        panic!("expected Files")
    };
    assert!(
        listing.entries.len() >= 2,
        "expected both fixture files, got {listing:?}"
    );
}

#[tokio::test]
async fn parity_find_text() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("find-text").await;
    let surface = direct_surface(&fixture);
    let mut ledger = fresh_ledger();
    let budget = SearchBudget {
        max_results: 100,
        max_files: 50,
        max_bytes: 1_000_000,
        deadline: Some(Duration::from_secs(5)),
    };
    let direct = surface
        .find(
            FindRequest {
                context: direct_context(&fixture),
                query: FindQuery::Text {
                    pattern: OwnedTextPattern::Literal("helper".to_owned()),
                    case_insensitive: false,
                    path_prefix: None,
                    budget,
                    max_file_bytes: 1_000_000,
                    with_preview: true,
                },
            },
            &mut ledger,
        )
        .expect("direct find text");
    let expected = convert_out::find_result_wire(direct).0;

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Find(FindQueryWire::Text {
            pattern: TextPatternWire::Literal("helper".to_owned()),
            case_insensitive: false,
            path_prefix: None,
            search_budget: SearchBudgetWire {
                max_results: 100,
                max_files: 50,
                max_bytes: 1_000_000,
                deadline_ms: Some(5_000),
            },
            max_file_bytes: 1_000_000,
            with_preview: true,
        }),
    )
    .await;
    let QueryResultWire::Find(actual) = ok_result(response) else {
        panic!("expected Find result")
    };
    assert_eq!(expected, actual);
    let FindResultWire::Text(result) = &actual else {
        panic!("expected Text")
    };
    assert!(
        !result.matches.is_empty(),
        "expected at least one 'helper' match, got {result:?}"
    );
}

#[tokio::test]
async fn parity_inspect() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("inspect").await;
    let surface = direct_surface(&fixture);
    let mut ledger = fresh_ledger();
    let direct = surface
        .inspect(
            InspectRequest {
                context: direct_context(&fixture),
                target: app_ts_target(),
                delivery: compact_direct_delivery(),
            },
            &mut ledger,
        )
        .expect("direct inspect");
    let expected = convert_out::projected_answer_wire(direct).0;

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Inspect(InspectWire {
            target: app_ts_target_wire(),
            delivery: compact_wire_delivery(),
        }),
    )
    .await;
    let QueryResultWire::Inspect(actual) = ok_result(response) else {
        panic!("expected Inspect result")
    };
    assert_eq!(expected, actual);
    assert!(
        actual
            .page
            .evidence
            .iter()
            .any(|item| matches!(item, EvidenceWire::CurrentSource(_))),
        "inspect on a real Resource should deliver CurrentSource, got {actual:?}"
    );
}

#[tokio::test]
async fn parity_relations() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("relations").await;
    let surface = direct_surface(&fixture);
    let direct = surface
        .relations(RelationsRequest {
            context: direct_context(&fixture),
            target: app_ts_target(),
            direction: RelationDirection::Both,
            kinds: Vec::new(),
        })
        .expect("direct relations");
    let expected = convert_out::relations_result_wire(direct);

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Relations(RelationsWire {
            target: app_ts_target_wire(),
            direction: RelationDirectionWire::Both,
            kinds: Vec::new(),
        }),
    )
    .await;
    let QueryResultWire::Relations(actual) = ok_result(response) else {
        panic!("expected Relations result")
    };
    assert_eq!(expected, actual);
    assert!(
        actual
            .answers
            .iter()
            .any(|answer| !answer.confirmed.is_empty()),
        "app.ts imports shared.ts: expected a confirmed relation, got {actual:?}"
    );
}

#[tokio::test]
async fn parity_impact() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("impact").await;
    let surface = direct_surface(&fixture);
    let mut ledger = fresh_ledger();
    let direct = surface
        .impact(
            ImpactRequest {
                context: direct_context(&fixture),
                target: shared_ts_target(),
                change: ChangeKind::Structural(ImpactIntent::ModuleMove),
                delivery: compact_direct_delivery(),
            },
            &mut ledger,
        )
        .expect("direct impact");
    let expected = convert_out::projected_answer_wire(direct).0;

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Impact(ImpactWire {
            target: shared_ts_target_wire(),
            change: ChangeKindWire::Structural(ImpactIntentWire::ModuleMove),
            delivery: compact_wire_delivery(),
        }),
    )
    .await;
    let QueryResultWire::Impact(actual) = ok_result(response) else {
        panic!("expected Impact result")
    };
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn parity_context_change() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("context-change").await;
    let surface = direct_surface(&fixture);
    let mut ledger = fresh_ledger();
    let direct = surface
        .context(
            ContextRequest {
                context: direct_context(&fixture),
                purpose: ContextPurpose::Change {
                    target: app_ts_target(),
                    change: None,
                    work_item: None,
                },
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge: ProjectionKnowledgeRefs::default(),
                delivery: compact_direct_delivery(),
            },
            &mut ledger,
        )
        .expect("direct context change");
    let expected = convert_out::projected_answer_wire(direct).0;

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Context(ContextWire::Change {
            target: app_ts_target_wire(),
            change: None,
            work_item: None,
            scope_layers: Vec::new(),
            directives: Vec::new(),
            knowledge_refs: ProjectionKnowledgeRefsWire::default(),
            delivery: compact_wire_delivery(),
        }),
    )
    .await;
    let QueryResultWire::Context(actual) = ok_result(response) else {
        panic!("expected Context result")
    };
    assert_eq!(expected, actual);
    assert!(
        actual
            .page
            .evidence
            .iter()
            .any(|item| matches!(item, EvidenceWire::CurrentSource(_))),
        "context Change on a real Resource should deliver CurrentSource, got {actual:?}"
    );
}

#[tokio::test]
async fn parity_context_resume() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("context-resume").await;
    let surface = direct_surface(&fixture);
    let mut ledger = fresh_ledger();
    let direct = surface
        .context(
            ContextRequest {
                context: direct_context(&fixture),
                purpose: ContextPurpose::Resume {
                    work_item: fixture.work_item,
                    target: None,
                },
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge: ProjectionKnowledgeRefs::default(),
                delivery: compact_direct_delivery(),
            },
            &mut ledger,
        )
        .expect("direct context resume");
    let expected = convert_out::projected_answer_wire(direct).0;

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Context(ContextWire::Resume {
            work_item: fixture.work_item,
            target: None,
            scope_layers: Vec::new(),
            directives: Vec::new(),
            knowledge_refs: ProjectionKnowledgeRefsWire::default(),
            delivery: compact_wire_delivery(),
        }),
    )
    .await;
    let QueryResultWire::Context(actual) = ok_result(response) else {
        panic!("expected Context result")
    };
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn parity_knowledge_rules() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("knowledge-rules").await;
    let surface = direct_surface(&fixture);
    let direct = surface
        .knowledge(KnowledgeRequest {
            context: direct_context(&fixture),
            query: KnowledgeQuery::Rules {
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge: ProjectionKnowledgeRefs::default(),
            },
        })
        .expect("direct knowledge rules");
    let expected = convert_out::knowledge_result_wire(direct);

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Knowledge(KnowledgeWire::Rules {
            scope_layers: Vec::new(),
            directives: Vec::new(),
            knowledge_refs: ProjectionKnowledgeRefsWire::default(),
        }),
    )
    .await;
    let QueryResultWire::Knowledge(actual) = ok_result(response) else {
        panic!("expected Knowledge result")
    };
    assert_eq!(expected, actual);
    let KnowledgeResultWire::Rules { evidence, .. } = &actual else {
        panic!("expected Rules")
    };
    assert!(
        evidence.iter().any(|item| matches!(
            item,
            EvidenceWire::Policy(resolved) if resolved.item.uid == fixture.policy
        )),
        "the inserted project Policy ({:?}) should be applicable, got {actual:?}",
        fixture.policy
    );
}

#[tokio::test]
async fn parity_knowledge_work_items() {
    let (fixture, mut connection, _server, _endpoint) =
        populated_fixture("knowledge-work-items").await;
    let surface = direct_surface(&fixture);
    let direct = surface
        .knowledge(KnowledgeRequest {
            context: direct_context(&fixture),
            query: KnowledgeQuery::WorkItems {
                statuses: vec![WorkItemStatus::Active],
                limit: NonZeroUsize::new(50).unwrap(),
            },
        })
        .expect("direct knowledge work items");
    let expected = convert_out::knowledge_result_wire(direct);

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
            statuses: vec![WorkItemStatusWire::Active],
            limit: NonZeroUsize::new(50).unwrap(),
        }),
    )
    .await;
    let QueryResultWire::Knowledge(actual) = ok_result(response) else {
        panic!("expected Knowledge result")
    };
    assert_eq!(expected, actual);
    let KnowledgeResultWire::WorkItems { items, .. } = &actual else {
        panic!("expected WorkItems")
    };
    assert_eq!(items.len(), 1, "expected the one started WorkItem");
}

#[tokio::test]
async fn parity_knowledge_lineage() {
    let (fixture, mut connection, _server, _endpoint) =
        populated_fixture("knowledge-lineage").await;
    let surface = direct_surface(&fixture);
    let direct = surface
        .knowledge(KnowledgeRequest {
            context: direct_context(&fixture),
            query: KnowledgeQuery::Lineage(LineageTarget::Decision(fixture.decision)),
        })
        .expect("direct knowledge lineage");
    let expected = convert_out::knowledge_result_wire(direct);

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Knowledge(KnowledgeWire::Lineage(LineageTargetWire::Decision(
            fixture.decision,
        ))),
    )
    .await;
    let QueryResultWire::Knowledge(actual) = ok_result(response) else {
        panic!("expected Knowledge result")
    };
    assert_eq!(expected, actual);
    assert!(matches!(actual, KnowledgeResultWire::DecisionLineage(_)));
}

#[tokio::test]
async fn parity_knowledge_handoffs() {
    let (fixture, mut connection, _server, _endpoint) =
        populated_fixture("knowledge-handoffs").await;
    let surface = direct_surface(&fixture);
    let direct = surface
        .knowledge(KnowledgeRequest {
            context: direct_context(&fixture),
            query: KnowledgeQuery::Handoffs {
                work_item: fixture.work_item,
                limit: NonZeroUsize::new(50).unwrap(),
            },
        })
        .expect("direct knowledge handoffs");
    let expected = convert_out::knowledge_result_wire(direct);

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Knowledge(KnowledgeWire::Handoffs {
            work_item: fixture.work_item,
            limit: NonZeroUsize::new(50).unwrap(),
        }),
    )
    .await;
    let QueryResultWire::Knowledge(actual) = ok_result(response) else {
        panic!("expected Knowledge result")
    };
    assert_eq!(expected, actual);
    let KnowledgeResultWire::Handoffs { handoffs, .. } = &actual else {
        panic!("expected Handoffs")
    };
    assert_eq!(handoffs.len(), 1, "expected the one recorded Handoff");
}

#[tokio::test]
async fn parity_structure() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("structure").await;
    let surface = direct_surface(&fixture);
    let direct = surface
        .structure(&brainprint_engine::boundary::StructuralSummaryRequest {
            workspace: fixture.workspace_id,
            grouping: GroupingSpec::ResourceRole,
            resource_scope: ResourceScope::default(),
            relation_kinds: Vec::new(),
            include_ungrouped: true,
            include_cycles: false,
            member_sample_limit: None,
        })
        .expect("direct structure");
    let expected = convert_out::structural_summary_wire(direct);

    let response = query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Structure(StructureWire {
            grouping: GroupingSpecWire::ResourceRole,
            resource_scope: SummaryResourceScopeWire::default(),
            relation_kinds: Vec::new(),
            include_ungrouped: true,
            include_cycles: false,
            member_sample_limit: None,
        }),
    )
    .await;
    let QueryResultWire::Structure(actual) = ok_result(response) else {
        panic!("expected Structure result")
    };
    assert_eq!(expected, actual);
    assert!(!actual.groups.is_empty(), "expected at least one group");
}

// ---------------------------------------------------------- instrumentation
//
// Acceptance 20-24: real Core stats, not code inspection.

#[tokio::test]
async fn source_reads_and_bytes_match_task_10_expectations() {
    let (fixture, _connection, _server, _endpoint) =
        populated_fixture("instrumentation-source").await;

    // find Target / relations / knowledge / structure: zero source reads.
    let zero_read_surface = direct_surface(&fixture);
    let mut ledger = fresh_ledger();
    zero_read_surface
        .find(
            FindRequest {
                context: direct_context(&fixture),
                query: FindQuery::Target {
                    target: app_ts_target(),
                    delivery: compact_direct_delivery(),
                },
            },
            &mut ledger,
        )
        .expect("find target");
    zero_read_surface
        .relations(RelationsRequest {
            context: direct_context(&fixture),
            target: app_ts_target(),
            direction: RelationDirection::Both,
            kinds: Vec::new(),
        })
        .expect("relations");
    zero_read_surface
        .knowledge(KnowledgeRequest {
            context: direct_context(&fixture),
            query: KnowledgeQuery::Rules {
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge: ProjectionKnowledgeRefs::default(),
            },
        })
        .expect("knowledge rules");
    zero_read_surface
        .structure(&brainprint_engine::boundary::StructuralSummaryRequest {
            workspace: fixture.workspace_id,
            grouping: GroupingSpec::ResourceRole,
            resource_scope: ResourceScope::default(),
            relation_kinds: Vec::new(),
            include_ungrouped: true,
            include_cycles: false,
            member_sample_limit: None,
        })
        .expect("structure");
    let mut resume_ledger = fresh_ledger();
    zero_read_surface
        .context(
            ContextRequest {
                context: direct_context(&fixture),
                purpose: ContextPurpose::Resume {
                    work_item: fixture.work_item,
                    target: None,
                },
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge: ProjectionKnowledgeRefs::default(),
                delivery: compact_direct_delivery(),
            },
            &mut resume_ledger,
        )
        .expect("context resume");
    let zero_read_stats = zero_read_surface.planner_stats();
    assert_eq!(
        zero_read_stats.source_file_reads, 0,
        "find Target/relations/knowledge/structure/context Resume must read no source: {zero_read_stats:?}"
    );
    assert_eq!(zero_read_stats.source_bytes, 0, "{zero_read_stats:?}");

    // inspect and context Change: equal, non-zero source bytes (#24
    // acceptance 21 -- measured, not merely asserted equal to each other).
    let inspect_surface = direct_surface(&fixture);
    let mut inspect_ledger = fresh_ledger();
    inspect_surface
        .inspect(
            InspectRequest {
                context: direct_context(&fixture),
                target: app_ts_target(),
                delivery: compact_direct_delivery(),
            },
            &mut inspect_ledger,
        )
        .expect("inspect");
    let inspect_stats = inspect_surface.planner_stats();

    let change_surface = direct_surface(&fixture);
    let mut change_ledger = fresh_ledger();
    change_surface
        .context(
            ContextRequest {
                context: direct_context(&fixture),
                purpose: ContextPurpose::Change {
                    target: app_ts_target(),
                    change: None,
                    work_item: None,
                },
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge: ProjectionKnowledgeRefs::default(),
                delivery: compact_direct_delivery(),
            },
            &mut change_ledger,
        )
        .expect("context change");
    let change_stats = change_surface.planner_stats();

    assert!(
        inspect_stats.source_file_reads > 0 && inspect_stats.source_bytes > 0,
        "inspect on a real Resource must read source: {inspect_stats:?}"
    );
    assert_eq!(
        inspect_stats.source_file_reads, change_stats.source_file_reads,
        "inspect vs context Change source_file_reads: {inspect_stats:?} vs {change_stats:?}"
    );
    assert_eq!(
        inspect_stats.source_bytes, change_stats.source_bytes,
        "inspect vs context Change source_bytes: {inspect_stats:?} vs {change_stats:?}"
    );
}

/// Acceptance 23: no semantic backend starts on a representative query
/// that Task 10's direct path starts none for either. Measured from the
/// real `semantic_publication` table -- the only durable trace a backend
/// start/publish leaves in `index.db` -- before and after every
/// representative operation, on both the direct and the daemon-wire path.
/// Never inferred from adapter code inspection.
#[tokio::test]
async fn no_semantic_backend_activity_on_structural_queries() {
    let (fixture, mut connection, _server, _endpoint) =
        populated_fixture("instrumentation-backend").await;

    fn semantic_publication_count(index_db: &Path) -> i64 {
        GraphStore::open(index_db)
            .expect("index.db")
            .connection()
            .query_row("SELECT COUNT(*) FROM semantic_publication", [], |row| {
                row.get(0)
            })
            .expect("count semantic_publication")
    }

    let before = semantic_publication_count(&fixture.paths.index_db);
    assert_eq!(before, 0, "fixture must start with no semantic activity");

    // Direct path.
    let surface = direct_surface(&fixture);
    let mut ledger = fresh_ledger();
    surface
        .find(
            FindRequest {
                context: direct_context(&fixture),
                query: FindQuery::Target {
                    target: app_ts_target(),
                    delivery: compact_direct_delivery(),
                },
            },
            &mut ledger,
        )
        .expect("find target");
    surface
        .relations(RelationsRequest {
            context: direct_context(&fixture),
            target: app_ts_target(),
            direction: RelationDirection::Both,
            kinds: Vec::new(),
        })
        .expect("relations");
    surface
        .structure(&brainprint_engine::boundary::StructuralSummaryRequest {
            workspace: fixture.workspace_id,
            grouping: GroupingSpec::ResourceRole,
            resource_scope: ResourceScope::default(),
            relation_kinds: Vec::new(),
            include_ungrouped: true,
            include_cycles: false,
            member_sample_limit: None,
        })
        .expect("structure");

    // Daemon-wire path, same operations.
    query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Find(FindQueryWire::Target {
            target: app_ts_target_wire(),
            delivery: compact_wire_delivery(),
        }),
    )
    .await;
    query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Relations(RelationsWire {
            target: app_ts_target_wire(),
            direction: RelationDirectionWire::Both,
            kinds: Vec::new(),
        }),
    )
    .await;
    query(
        &mut connection,
        fixture.workspace_id,
        QueryOperationWire::Structure(StructureWire {
            grouping: GroupingSpecWire::ResourceRole,
            resource_scope: SummaryResourceScopeWire::default(),
            relation_kinds: Vec::new(),
            include_ungrouped: true,
            include_cycles: false,
            member_sample_limit: None,
        }),
    )
    .await;

    let after = semantic_publication_count(&fixture.paths.index_db);
    assert_eq!(
        after, 0,
        "no representative structural query may start/publish a semantic backend"
    );
}

// -------------------------------------------------------- workspace/binding

#[tokio::test]
async fn ambiguous_locator_is_explicit_never_first_match() {
    let home = TestHome::create("ambiguous-home");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;
    let workspace = TestHome::create("ambiguous-ws");

    let mut connection = ready_connection(&endpoint).await;
    let workspace_id = init_workspace(&mut connection, workspace.path()).await;

    let registry = GlobalRegistry::open(&global_paths.global_db).expect("registry");
    let registered = registry
        .get_workspace(workspace_id)
        .expect("lookup")
        .expect("registered");
    // The exact stored (canonicalized) locator, not the raw path Init was
    // given: `find_by_locator` must see both rows at the identical
    // locator representation for this to be a genuine ambiguity rather
    // than a canonicalization mismatch.
    let stored_locator = registered.locator.clone();
    let second = WorkspaceId::generate();
    registry
        .register_workspace(second, registered.project_id, &stored_locator, false)
        .expect("register a second Workspace at the same locator");

    let request = Request::Query(QueryRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        workspace: WorkspaceSelectorWire::Locator {
            path: stored_locator.to_string_lossy().into_owned(),
        },
        correlation: None,
        operation: QueryOperationWire::Find(FindQueryWire::Target {
            target: app_ts_target_wire(),
            delivery: compact_wire_delivery(),
        }),
    });
    let Response::Query(response) = send(&mut connection, request).await else {
        panic!("expected a Query response")
    };
    let QueryOutcomeWire::Err(error) = response.outcome else {
        panic!("an ambiguous locator must be a typed error, not a first-match pick")
    };
    assert_eq!(error.code, QueryErrorCodeWire::WorkspaceAmbiguous);
}

#[tokio::test]
async fn binding_mismatch_is_explicit_not_silently_repaired() {
    let home = TestHome::create("binding-home");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;

    let workspace_a = TestHome::create("binding-a");
    let workspace_b = TestHome::create("binding-b");

    let mut connection = ready_connection(&endpoint).await;
    let id_a = init_workspace(&mut connection, workspace_a.path()).await;
    let id_b = init_workspace(&mut connection, workspace_b.path()).await;
    assert_ne!(id_a, id_b);

    let paths_a = WorkspacePaths::from_root(workspace_a.path());
    let paths_b = WorkspacePaths::from_root(workspace_b.path());
    // Corrupt A's binding by swapping in B's data files, *before* any
    // query ever opens A's worker (the worker's CoreQuerySurface is
    // opened once and reused -- swapping after the fact would prove
    // nothing about a fresh bind, #24 §14).
    fs::copy(&paths_b.workspace_db, &paths_a.workspace_db).expect("swap workspace.db");
    fs::copy(&paths_b.index_db, &paths_a.index_db).expect("swap index.db");

    let response = query(
        &mut connection,
        id_a,
        QueryOperationWire::Find(FindQueryWire::Target {
            target: app_ts_target_wire(),
            delivery: compact_wire_delivery(),
        }),
    )
    .await;
    let QueryOutcomeWire::Err(error) = response.outcome else {
        panic!("a binding mismatch must never be silently repaired or auto-initialized")
    };
    assert_eq!(error.code, QueryErrorCodeWire::WorkspaceBindingMismatch);

    // The daemon itself must still be alive and servable.
    assert!(matches!(
        send(
            &mut connection,
            Request::Status(brainprint_core::protocol::StatusRequest)
        )
        .await,
        Response::Status(_)
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
        .expect("git");
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

/// Task-11-level (through the real daemon protocol) two-worktree
/// isolation: one Project (shared project-home), two Workspaces, each
/// with its own WorkItem lifecycle state that the other never sees.
#[tokio::test]
async fn two_worktrees_stay_isolated_through_the_daemon_protocol() {
    let home = TestHome::create("worktree-home");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;

    let main = TestHome::create("worktree-main");
    let secondary = TestHome::create("worktree-secondary");
    fs::remove_dir_all(secondary.path()).expect("worktree target must not exist yet");
    fs::create_dir_all(main.path().join("src")).expect("dirs");
    fs::write(main.path().join("src/shared.ts"), SHARED_TS).expect("file");
    run_git(&["init", "-q"], main.path());
    run_git(&["add", "."], main.path());
    run_git(&["commit", "-q", "-m", "init"], main.path());
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

    let mut connection = ready_connection(&endpoint).await;
    let id_main = init_workspace(&mut connection, main.path()).await;
    let id_secondary = init_workspace(&mut connection, secondary.path()).await;
    assert_ne!(id_main, id_secondary);

    let registry = GlobalRegistry::open(&global_paths.global_db).expect("registry");
    let project_of = |workspace_id: WorkspaceId| {
        registry
            .get_workspace(workspace_id)
            .expect("lookup")
            .expect("registered")
            .project_id
    };
    assert_eq!(
        project_of(id_main),
        project_of(id_secondary),
        "two worktrees of one Project share project-home"
    );

    let main_paths = WorkspacePaths::from_root(main.path());
    BaselineScan::open(&main_paths.index_db)
        .expect("index.db")
        .run_initial_scan(main.path(), &WorkspaceConfig::default(), "wt-rev-1")
        .expect("baseline scan main");
    let work = WorkRuntime::open(id_main, &main_paths.workspace_db, &main_paths.index_db)
        .expect("work runtime");
    let created = work
        .create(&NewWorkItem {
            source_kind: WorkItemSourceKind::Issue,
            source_ref: Some("#24".to_owned()),
            title: None,
            goal: "main-only work".to_owned(),
        })
        .expect("create");
    work.start(
        created.uid,
        &StartObservation {
            head: None,
            dirty: DirtyObservation::Unknown,
            preexisting_dirty: Vec::new(),
            owner_agent: Some("task11-verification".to_owned()),
        },
    )
    .expect("start");

    let main_items = query(
        &mut connection,
        id_main,
        QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
            statuses: vec![WorkItemStatusWire::Active],
            limit: NonZeroUsize::new(50).unwrap(),
        }),
    )
    .await;
    let QueryResultWire::Knowledge(KnowledgeResultWire::WorkItems {
        items: main_items, ..
    }) = ok_result(main_items)
    else {
        panic!("expected WorkItems")
    };
    assert_eq!(main_items.len(), 1, "main sees its own WorkItem");

    let secondary_items = query(
        &mut connection,
        id_secondary,
        QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
            statuses: vec![WorkItemStatusWire::Active],
            limit: NonZeroUsize::new(50).unwrap(),
        }),
    )
    .await;
    let QueryResultWire::Knowledge(KnowledgeResultWire::WorkItems {
        items: secondary_items,
        ..
    }) = ok_result(secondary_items)
    else {
        panic!("expected WorkItems")
    };
    assert!(
        secondary_items.is_empty(),
        "the secondary worktree must never see the main worktree's WorkItem, got {secondary_items:?}"
    );
}
