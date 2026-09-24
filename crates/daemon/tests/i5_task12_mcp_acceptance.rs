//! #25 (I5 Task 12) MCP semantic-parity / error / economy acceptance.
//!
//! Drives the real `brainprint_mcp::BrainprintMcp` tool methods
//! in-process (no stdio, no subprocess) against a real daemon `Server`
//! and a populated Workspace fixture -- the same shape Task 11's own
//! `crates/daemon/src/query/verification.rs` uses. `brainprint-mcp`
//! itself has zero dependency on `brainprint-engine`/`brainprint-daemon`
//! (#25 "Crate boundary"); this test lives in `brainprint-daemon`
//! instead, which already depends on both, and dev-depends on
//! `brainprint-mcp` purely to call its tool methods -- the opposite of
//! the forbidden direction, and no part of the shipped `brainprint-mcp`
//! binary depends on this crate.
//!
//! Parity is checked by comparing the real daemon wire `QueryResultWire`
//! (sent as a plain `Request::Query`, exactly as `brainprint-cli` would)
//! against the same wire type recovered from the MCP tool's
//! `structured_content.payload` -- since the MCP envelope carries that
//! exact value unchanged (#25 "MCP result contract": "Do not stringify
//! Debug output... Preserve Task 11 meaning exactly"), this is a direct
//! equality check on the same type, not a second re-derivation.

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
use brainprint_daemon::{runtime_paths, server::Server};
use brainprint_engine::{
    config::WorkspaceConfig,
    graph::GraphStore,
    knowledge::{
        DirtyObservation, KnowledgeScope, NewDecision, NewPolicy, NewWorkItem, PriorityClass,
        ProjectKnowledgeStore, ProtectionClass, Provenance, SourceKind, StartObservation,
        WorkHandoff, WorkItemSourceKind, WorkRuntime,
    },
    paths::{GlobalPaths, WorkspacePaths},
    registry::GlobalRegistry,
    scan::BaselineScan,
};
use brainprint_mcp::{BrainprintMcp, tools::inspect::InspectParams};
use rmcp::handler::server::wrapper::Parameters;
use serde_json::json;

// --------------------------------------------------------------- harness

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestHome(PathBuf);

impl TestHome {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-i5-task12-mcp-{label}-{}-{sequence}",
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

async fn direct_query(
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

// ------------------------------------------------------------- fixture

const SHARED_TS: &str = "export function helper(): number {\n  return 42;\n}\n";
const APP_TS: &str = "import { helper } from \"./shared\";\n\nexport function run(): number {\n  return helper();\n}\n";

/// Same shape as Task 11's own populated fixture: two real TypeScript
/// files with a real import relation, one project Policy, one project
/// Decision, one started WorkItem with one Handoff.
struct Populated {
    _home: TestHome,
    workspace: TestHome,
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
            scope: KnowledgeScope::project(),
            policy_key: Some("task12-mcp-policy".to_owned()),
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
            scope: KnowledgeScope::project(),
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
            source_ref: Some("#25".to_owned()),
            title: None,
            goal: "verify Task 12 MCP parity".to_owned(),
        })
        .expect("create work item");
    work.start(
        created.uid,
        &StartObservation {
            head: None,
            dirty: DirtyObservation::Unknown,
            preexisting_dirty: Vec::new(),
            owner_agent: Some("task12-mcp-acceptance".to_owned()),
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
        workspace,
        global: global_paths,
        workspace_id,
        paths,
        work_item: created.uid,
        policy,
        decision,
    };
    (fixture, connection, server, endpoint)
}

/// Points the in-process `BrainprintMcp` at this fixture's own isolated
/// daemon endpoint, not the real ambient one (`brainprint_mcp::daemon`'s
/// `endpoint_override` testability seam) -- there is no safe way to
/// override the real environment's `$HOME`/`$XDG_RUNTIME_DIR` across
/// concurrently-running tests.
fn mcp_endpoint(fixture_global: &GlobalPaths) -> brainprint_core::protocol::EndpointPaths {
    brainprint_core::protocol::EndpointPaths::from_runtime_root(fixture_global.runtime_root())
}

fn mcp_server(fixture: &Populated) -> BrainprintMcp {
    BrainprintMcp::with_endpoint(
        fixture.workspace.path().to_path_buf(),
        mcp_endpoint(&fixture.global),
    )
}

/// The MCP wrapper's `payload` field, deserialized back into the exact
/// Task 11 wire type -- never a stringified/Debug re-interpretation.
fn payload<T: serde::de::DeserializeOwned>(result: &rmcp::model::CallToolResult) -> T {
    let structured = result
        .structured_content
        .as_ref()
        .expect("MCP result must carry structured content");
    assert_eq!(
        structured["protocol_version"], PROTOCOL_VERSION,
        "MCP envelope must carry the real Task 11 protocol version"
    );
    serde_json::from_value(structured["payload"].clone()).expect("payload should decode")
}

fn workspace_json(fixture: &Populated) -> serde_json::Value {
    json!({ "workspace_path": fixture.workspace.path().to_string_lossy() })
}

// ------------------------------------------------------- operation parity

#[tokio::test]
async fn parity_find_target() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("find-target").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Find(FindQueryWire::Target {
                target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                    "src/app.ts".to_owned(),
                )),
                delivery: compact_delivery(),
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({"mode": "target", "resource_path": "src/app.ts"}),
    );
    let result = mcp
        .find(Parameters(from_json(params)))
        .await
        .expect("brainprint.find target should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_find_files() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("find-files").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
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
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({"mode": "files", "recursive": true, "limit": 50}),
    );
    let result = mcp
        .find(Parameters(from_json(params)))
        .await
        .expect("brainprint.find files should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_find_text() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("find-text").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Find(FindQueryWire::Text {
                pattern: TextPatternWire::Literal("helper".to_owned()),
                case_insensitive: false,
                path_prefix: None,
                search_budget: SearchBudgetWire {
                    max_results: 50,
                    max_files: 500,
                    max_bytes: 8 * 1024 * 1024,
                    deadline_ms: Some(1_000),
                },
                max_file_bytes: 1024 * 1024,
                with_preview: true,
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({"mode": "text", "pattern": "helper", "search_budget_profile": "compact"}),
    );
    let result = mcp
        .find(Parameters(from_json(params)))
        .await
        .expect("brainprint.find text should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
    let QueryResultWire::Find(FindResultWire::Text(text)) = &via_mcp else {
        panic!("expected Text result")
    };
    assert!(!text.matches.is_empty(), "expected a 'helper' match");
}

#[tokio::test]
async fn parity_inspect() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("inspect").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Inspect(InspectWire {
                target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                    "src/app.ts".to_owned(),
                )),
                delivery: compact_delivery(),
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(&mut params, json!({"resource_path": "src/app.ts"}));
    let result: InspectParams = from_json(params);
    let result = mcp
        .inspect(Parameters(result))
        .await
        .expect("brainprint.inspect should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
    let QueryResultWire::Inspect(answer) = &via_mcp else {
        panic!("expected Inspect result")
    };
    assert!(
        answer
            .page
            .evidence
            .iter()
            .any(|item| matches!(item, EvidenceWire::CurrentSource(_))),
        "inspect on a real Resource should deliver CurrentSource"
    );
}

#[tokio::test]
async fn parity_relations_direct() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("relations-direct").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Relations(RelationsWire {
                target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                    "src/app.ts".to_owned(),
                )),
                direction: RelationDirectionWire::Both,
                kinds: Vec::new(),
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({"mode": "direct", "resource_path": "src/app.ts"}),
    );
    let result = mcp
        .relations(Parameters(from_json(params)))
        .await
        .expect("brainprint.relations direct should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_relations_impact() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("relations-impact").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Impact(ImpactWire {
                target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                    "src/shared.ts".to_owned(),
                )),
                change: ChangeKindWire::Structural(ImpactIntentWire::ModuleMove),
                delivery: compact_delivery(),
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({
            "mode": "impact",
            "resource_path": "src/shared.ts",
            "change": {"kind": "structural", "intent": "module_move"},
        }),
    );
    let result = mcp
        .relations(Parameters(from_json(params)))
        .await
        .expect("brainprint.relations impact should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_context_change() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("context-change").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Context(ContextWire::Change {
                target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                    "src/app.ts".to_owned(),
                )),
                change: None,
                work_item: None,
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge_refs: ProjectionKnowledgeRefsWire::default(),
                delivery: compact_delivery(),
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({"mode": "change", "resource_path": "src/app.ts"}),
    );
    let result = mcp
        .context(Parameters(from_json(params)))
        .await
        .expect("brainprint.context change should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_context_resume() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("context-resume").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Context(ContextWire::Resume {
                work_item: fixture.work_item,
                target: None,
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge_refs: ProjectionKnowledgeRefsWire::default(),
                delivery: compact_delivery(),
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({"mode": "resume", "work_item": fixture.work_item.to_string()}),
    );
    let result = mcp
        .context(Parameters(from_json(params)))
        .await
        .expect("brainprint.context resume should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_context_rules() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("context-rules").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Knowledge(KnowledgeWire::Rules {
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge_refs: ProjectionKnowledgeRefsWire::default(),
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(&mut params, json!({"mode": "rules"}));
    let result = mcp
        .context(Parameters(from_json(params)))
        .await
        .expect("brainprint.context rules should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
    let QueryResultWire::Knowledge(KnowledgeResultWire::Rules { evidence, .. }) = &via_mcp else {
        panic!("expected Rules")
    };
    assert!(
        evidence.iter().any(
            |item| matches!(item, EvidenceWire::Policy(resolved) if resolved.item.uid == fixture.policy)
        ),
        "the inserted Policy should be applicable"
    );
}

#[tokio::test]
async fn parity_context_work_items() {
    let (fixture, mut connection, _server, _endpoint) =
        populated_fixture("context-work-items").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
                statuses: vec![WorkItemStatusWire::Active],
                limit: NonZeroUsize::new(50).unwrap(),
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({"mode": "work_items", "statuses": ["active"]}),
    );
    let result = mcp
        .context(Parameters(from_json(params)))
        .await
        .expect("brainprint.context work_items should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_context_lineage() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("context-lineage").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Knowledge(KnowledgeWire::Lineage(LineageTargetWire::Decision(
                fixture.decision,
            ))),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({"mode": "lineage", "lineage_of": "decision", "id": fixture.decision.to_string()}),
    );
    let result = mcp
        .context(Parameters(from_json(params)))
        .await
        .expect("brainprint.context lineage should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_context_handoffs() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("context-handoffs").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Knowledge(KnowledgeWire::Handoffs {
                work_item: fixture.work_item,
                limit: NonZeroUsize::new(20).unwrap(),
            }),
        )
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(
        &mut params,
        json!({"mode": "handoffs", "work_item": fixture.work_item.to_string()}),
    );
    let result = mcp
        .context(Parameters(from_json(params)))
        .await
        .expect("brainprint.context handoffs should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_context_structure() {
    let (fixture, mut connection, _server, _endpoint) =
        populated_fixture("context-structure").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
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
        .await,
    );

    let mut params = workspace_json(&fixture);
    merge(&mut params, json!({"mode": "structure"}));
    let result = mcp
        .context(Parameters(from_json(params)))
        .await
        .expect("brainprint.context structure should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    assert_eq!(direct, via_mcp);
}

#[tokio::test]
async fn parity_context_status() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("context-status").await;
    let mcp = mcp_server(&fixture);

    let Response::Status(direct) = send(
        &mut connection,
        Request::Status(brainprint_core::protocol::StatusRequest),
    )
    .await
    else {
        panic!("expected a Status response")
    };

    let mut params = json!({"mode": "status"});
    let _ = &mut params;
    let result = mcp
        .context(Parameters(from_json(json!({"mode": "status"}))))
        .await
        .expect("brainprint.context status should succeed");
    let via_mcp: brainprint_core::protocol::StatusResponse = payload(&result);
    assert_eq!(direct, via_mcp);
}

// ---------------------------------------------------------- error/currentness

#[tokio::test]
async fn not_initialized_stays_typed() {
    let home = TestHome::create("not-initialized-home");
    let global_paths = home.global_paths();
    let (_server, _endpoint) = start_server(&global_paths).await;
    let workspace = TestHome::create("not-initialized-ws");
    let mcp =
        BrainprintMcp::with_endpoint(workspace.path().to_path_buf(), mcp_endpoint(&global_paths));

    let result = mcp
        .find(Parameters(from_json(json!({
            "mode": "target",
            "resource_path": "src/app.ts",
            "workspace_path": workspace.path().to_string_lossy(),
        }))))
        .await
        .expect("tools/call must not fail as an MCP protocol error");
    assert_eq!(result.is_error, Some(true));
    let error: QueryErrorWire = payload(&result);
    assert_eq!(error.code, QueryErrorCodeWire::NotInitialized);
}

#[tokio::test]
async fn workspace_ambiguous_never_first_match() {
    let (fixture, mut connection, _server, _endpoint) =
        populated_fixture("workspace-ambiguous").await;
    let mcp = mcp_server(&fixture);

    let registry = GlobalRegistry::open(&fixture.global.global_db).expect("registry");
    let registered = registry
        .get_workspace(fixture.workspace_id)
        .expect("lookup")
        .expect("registered");
    let stored_locator = registered.locator.clone();
    registry
        .register_workspace(
            WorkspaceId::generate(),
            registered.project_id,
            &stored_locator,
            false,
        )
        .expect("register a second Workspace at the same locator");

    let result = mcp
        .find(Parameters(from_json(json!({
            "mode": "target",
            "resource_path": "src/app.ts",
            "workspace_path": stored_locator.to_string_lossy(),
        }))))
        .await
        .expect("tools/call must not fail as an MCP protocol error");
    assert_eq!(result.is_error, Some(true));
    let error: QueryErrorWire = payload(&result);
    assert_eq!(error.code, QueryErrorCodeWire::WorkspaceAmbiguous);

    // Daemon-side confirmation the connection is unaffected.
    assert!(matches!(
        send(
            &mut connection,
            Request::Status(brainprint_core::protocol::StatusRequest)
        )
        .await,
        Response::Status(_)
    ));
}

#[tokio::test]
async fn budget_too_small_stays_typed() {
    let (fixture, _connection, _server, _endpoint) = populated_fixture("budget-too-small").await;
    let mcp = mcp_server(&fixture);

    // A syntactically valid (non-zero) but real daemon-side too-small
    // budget -- distinct from an adapter-side `max_items: 0` rejection,
    // which never reaches the daemon at all.
    let result = mcp
        .inspect(Parameters(from_json(json!({
            "resource_path": "src/app.ts",
            "workspace_path": fixture.workspace.path().to_string_lossy(),
            "max_items": 1,
            "max_bytes": 1,
        }))))
        .await
        .expect("tools/call must not fail as an MCP protocol error");
    assert_eq!(result.is_error, Some(true));
    let error: QueryErrorWire = payload(&result);
    assert_eq!(error.code, QueryErrorCodeWire::BudgetTooSmall);
}

#[tokio::test]
async fn continuation_mismatch_stays_typed() {
    let (fixture, _connection, _server, _endpoint) =
        populated_fixture("continuation-mismatch").await;
    let mcp = mcp_server(&fixture);

    // A structurally valid but stale/foreign continuation: every
    // identity field is well-formed, but it names a generation this
    // fresh Workspace never had.
    let bogus_continuation = json!({
        "workspace_id": fixture.workspace_id.to_string(),
        "index_incarnation_id": fixture.workspace_id.to_string(),
        "workspace_revision": "not-a-real-revision",
        "generation_no": 999_999,
        "generation_basis_revision": "not-a-real-basis",
        "request_fingerprint": "0".repeat(64),
        "projection_fingerprint": "0".repeat(64),
        "budget": {"max_items": 16, "max_bytes": 16384, "max_tokens": null},
        "next": {"tier": 0, "depth": 0, "identity": "0".repeat(64)},
    });

    let result = mcp
        .inspect(Parameters(from_json(json!({
            "resource_path": "src/app.ts",
            "workspace_path": fixture.workspace.path().to_string_lossy(),
            "continuation": bogus_continuation,
        }))))
        .await
        .expect("tools/call must not fail as an MCP protocol error");
    assert_eq!(result.is_error, Some(true));
    let error: QueryErrorWire = payload(&result);
    assert_eq!(error.code, QueryErrorCodeWire::ContinuationMismatch);
}

#[tokio::test]
async fn result_too_large_stays_typed() {
    // `find text` under `search_budget_profile: wide` caps at 500
    // matches (#25's locked profile, never overridden), and each
    // match's own JSON footprint measures ~390 bytes -- nowhere near
    // 1 MiB even at the cap. `relations direct` has no such cap at all
    // (#25 "brainprint.relations": "one anchor, one hop, unpaged, no
    // source" -- the whole `RelationIndex` answer), so many resources
    // all importing one shared module reaches it reliably: build enough
    // real confirmed Imports relations that the unpaged answer clears
    // the 1 MiB frame on its own.
    let home = TestHome::create("result-too-large-home");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;
    let workspace = TestHome::create("result-too-large-ws");
    fs::create_dir_all(workspace.path().join("src")).expect("src dir");
    fs::write(workspace.path().join("src/shared.ts"), SHARED_TS).expect("shared.ts");
    for index in 0..4000 {
        fs::write(
            workspace.path().join(format!("importer{index}.ts")),
            "import { helper } from \"./src/shared\";\n",
        )
        .expect("fixture file should write");
    }
    let mut connection = ready_connection(&endpoint).await;
    init_workspace(&mut connection, workspace.path()).await;
    let paths = WorkspacePaths::from_root(workspace.path());
    BaselineScan::open(&paths.index_db)
        .expect("index.db")
        .run_initial_scan(workspace.path(), &WorkspaceConfig::default(), "rev-1")
        .expect("baseline scan");

    let mcp =
        BrainprintMcp::with_endpoint(workspace.path().to_path_buf(), mcp_endpoint(&global_paths));
    let result = mcp
        .relations(Parameters(from_json(json!({
            "mode": "direct",
            "resource_path": "src/shared.ts",
            "direction": "incoming",
            "workspace_path": workspace.path().to_string_lossy(),
        }))))
        .await
        .expect("tools/call must not fail as an MCP protocol error");
    assert_eq!(result.is_error, Some(true));
    let error: QueryErrorWire = payload(&result);
    assert_eq!(error.code, QueryErrorCodeWire::ResultTooLarge);
}

/// #25 acceptance 28: Text is never an automatic fallback from a
/// structured miss.
#[tokio::test]
async fn find_target_miss_never_falls_back_to_text() {
    let (fixture, _connection, _server, _endpoint) = populated_fixture("no-auto-fallback").await;
    let mcp = mcp_server(&fixture);

    let result = mcp
        .find(Parameters(from_json(json!({
            "mode": "target",
            "resource_path": "src/does-not-exist.ts",
            "workspace_path": fixture.workspace.path().to_string_lossy(),
        }))))
        .await
        .expect("brainprint.find target should succeed as a valid NotFound answer");
    assert_eq!(
        result.is_error,
        Some(false),
        "a structural miss is a valid, typed Ok answer, not a tool error"
    );
    let via_mcp: QueryResultWire = payload(&result);
    let QueryResultWire::Find(FindResultWire::Target(_)) = via_mcp else {
        panic!("expected Find::Target, not a Text result -- an automatic fallback occurred")
    };
}

// ------------------------------------------------------------- economy

/// #25 acceptance 29-31: the MCP adapter itself performs zero project
/// source reads and zero DB queries, and causes no semantic backend
/// starts, measured from real Core stats / real DB state -- not code
/// inspection.
#[tokio::test]
async fn mcp_adapter_causes_zero_source_reads_zero_db_reads_zero_backend_starts() {
    let (fixture, _connection, _server, _endpoint) = populated_fixture("economy").await;
    let mcp = mcp_server(&fixture);

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
    assert_eq!(before, 0);

    // A representative call per tool, all through the MCP adapter only
    // -- the adapter process itself never touches index.db/project.db/
    // workspace.db/global.db (#25 "Zero source / DB access"; verified
    // here from the real DB, and by the fact that `brainprint-mcp`
    // links no SQLite/engine dependency at all -- `cargo tree` on the
    // shipped binary target has neither).
    mcp.find(Parameters(from_json(json!({
        "mode": "target",
        "resource_path": "src/app.ts",
        "workspace_path": fixture.workspace.path().to_string_lossy(),
    }))))
    .await
    .expect("find target");
    mcp.relations(Parameters(from_json(json!({
        "mode": "direct",
        "resource_path": "src/app.ts",
        "workspace_path": fixture.workspace.path().to_string_lossy(),
    }))))
    .await
    .expect("relations direct");
    mcp.context(Parameters(from_json(json!({
        "mode": "structure",
        "workspace_path": fixture.workspace.path().to_string_lossy(),
    }))))
    .await
    .expect("context structure");

    let after = semantic_publication_count(&fixture.paths.index_db);
    assert_eq!(after, 0, "no MCP call may start/publish a semantic backend");
}

/// #25 acceptance 32: inspect source bytes via MCP equal the direct
/// Task 11 result -- the same real `CurrentSource` bytes, not a
/// re-derived summary.
#[tokio::test]
async fn inspect_source_bytes_via_mcp_equal_direct_task11_result() {
    let (fixture, mut connection, _server, _endpoint) =
        populated_fixture("inspect-source-bytes").await;
    let mcp = mcp_server(&fixture);

    let direct = ok_result(
        direct_query(
            &mut connection,
            fixture.workspace_id,
            QueryOperationWire::Inspect(InspectWire {
                target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                    "src/app.ts".to_owned(),
                )),
                delivery: compact_delivery(),
            }),
        )
        .await,
    );
    let QueryResultWire::Inspect(direct_answer) = &direct else {
        panic!("expected Inspect")
    };
    let direct_source = current_source_text(direct_answer);

    let result = mcp
        .inspect(Parameters(from_json(json!({
            "resource_path": "src/app.ts",
            "workspace_path": fixture.workspace.path().to_string_lossy(),
        }))))
        .await
        .expect("brainprint.inspect should succeed");
    let via_mcp: QueryResultWire = payload(&result);
    let QueryResultWire::Inspect(mcp_answer) = &via_mcp else {
        panic!("expected Inspect")
    };
    let mcp_source = current_source_text(mcp_answer);

    assert_eq!(direct_source, mcp_source);
    assert!(!direct_source.is_empty(), "expected real source bytes");
}

fn current_source_text(answer: &ProjectedAnswerWire) -> String {
    answer
        .page
        .evidence
        .iter()
        .find_map(|item| match item {
            EvidenceWire::CurrentSource(range) => Some(range.source.clone()),
            _ => None,
        })
        .expect("expected a CurrentSource evidence item")
}

// ------------------------------------------------------------- helpers

fn compact_delivery() -> DeliveryWire {
    DeliveryWire {
        budget: DeliveryBudgetWire {
            max_items: NonZeroUsize::new(64),
            max_bytes: NonZeroUsize::new(64 * 1024),
        },
        continuation: None,
        retention: RetentionWire::Disabled,
    }
}

fn merge(base: &mut serde_json::Value, extra: serde_json::Value) {
    let (serde_json::Value::Object(base), serde_json::Value::Object(extra)) = (base, extra) else {
        panic!("merge expects two JSON objects")
    };
    base.extend(extra);
}

fn from_json<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).expect("MCP tool params should deserialize")
}

// ------------------------------------------------------- performance record

/// #25 "Performance/overhead record": representative byte/latency
/// measurements for one call, printed for the completion record. Not
/// asserted against a threshold (no product requirement sets one) --
/// this test's job is to produce real numbers, not to gate on them.
#[tokio::test]
async fn representative_byte_and_latency_measurements() {
    let (fixture, mut connection, _server, _endpoint) = populated_fixture("perf-record").await;
    let mcp = mcp_server(&fixture);

    let mcp_request = json!({
        "mode": "target",
        "resource_path": "src/app.ts",
        "workspace_path": fixture.workspace.path().to_string_lossy(),
    });
    let mcp_request_bytes = serde_json::to_vec(&mcp_request).unwrap().len();

    let mcp_start = std::time::Instant::now();
    let result = mcp
        .find(Parameters(from_json(mcp_request)))
        .await
        .expect("brainprint.find target should succeed");
    let mcp_latency = mcp_start.elapsed();
    let mcp_result_bytes = serde_json::to_vec(result.structured_content.as_ref().unwrap())
        .unwrap()
        .len();

    let daemon_request = QueryRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        workspace: WorkspaceSelectorWire::Id {
            workspace_id: fixture.workspace_id,
        },
        correlation: None,
        operation: QueryOperationWire::Find(FindQueryWire::Target {
            target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                "src/app.ts".to_owned(),
            )),
            delivery: compact_delivery(),
        }),
    };
    let daemon_request_bytes = serde_json::to_vec(&daemon_request).unwrap().len();

    let daemon_start = std::time::Instant::now();
    let direct = send(&mut connection, Request::Query(daemon_request)).await;
    let daemon_latency = daemon_start.elapsed();
    let daemon_response_bytes = serde_json::to_vec(&direct).unwrap().len();

    let skill_bytes = fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../integrations/brainprint/SKILL.md"
    ))
    .map(|content| content.len())
    .unwrap_or(0);

    println!(
        "MEASURED mcp_request_bytes={mcp_request_bytes} mcp_result_bytes={mcp_result_bytes} \
         daemon_request_bytes={daemon_request_bytes} daemon_response_bytes={daemon_response_bytes} \
         adapter_added_bytes={} mcp_latency_us={} daemon_query_latency_us={} skill_bytes={skill_bytes}",
        mcp_result_bytes.saturating_sub(daemon_response_bytes),
        mcp_latency.as_micros(),
        daemon_latency.as_micros(),
    );
    assert!(matches!(result.is_error, Some(false)));
    assert!(matches!(direct, Response::Query(_)));
}
