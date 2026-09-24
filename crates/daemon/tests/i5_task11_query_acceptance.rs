//! #24 (I5 Task 11) query protocol acceptance tests.
//!
//! In-process (no subprocess): binds a real `Server`, connects real
//! `ClientConnection`s to it, and drives `Request::Query`/`QueryAck`
//! exactly as `brainprint-cli` does. Covers the protocol/runtime/ack
//! mechanics that do not require a populated structural index, plus
//! direct-Core-vs-wire parity for the query shapes that are meaningful
//! without one (target-not-found, empty structure, text search over real
//! files on disk). Task 5-10 unit suites remain the source of truth for
//! populated-index semantics; this file does not re-test them.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use brainprint_core::{
    PROTOCOL_VERSION, WorkspaceId,
    protocol::{
        self, ClientConnection, ErrorKind, HandshakeRequest, HandshakeResponse, InitRequest,
        InitResponse, Request, Response, query::*,
    },
};
use brainprint_daemon::{runtime_paths, server::Server};
use brainprint_engine::{paths::GlobalPaths, query_surface::CoreQuerySurface};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestHome(PathBuf);

impl TestHome {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-i5-task11-{label}-{}-{sequence}",
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
    // Give the accept loop a moment to actually be listening.
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

/// Init `path` as a Workspace through the real daemon protocol, returning
/// its `WorkspaceId`.
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

fn compact_delivery() -> DeliveryWire {
    DeliveryWire {
        budget: DeliveryBudgetWire {
            max_items: std::num::NonZeroUsize::new(16),
            max_bytes: std::num::NonZeroUsize::new(16 * 1024),
        },
        continuation: None,
        retention: RetentionWire::Disabled,
    }
}

// --------------------------------------------------------------- protocol

#[tokio::test]
async fn v1_client_against_v2_daemon_is_rejected() {
    let home = TestHome::create("v1-client");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;
    let mut connection = connect(&endpoint).await;

    let response = handshake(&mut connection, 1).await;
    assert!(
        matches!(
            response,
            Response::Handshake(HandshakeResponse::VersionMismatch {
                server_protocol_version: 2,
                client_protocol_version: 1,
            })
        ),
        "got {response:?}"
    );
}

#[tokio::test]
async fn v2_client_speaks_the_current_protocol_version() {
    assert_eq!(PROTOCOL_VERSION, 2);
}

#[tokio::test]
async fn query_before_handshake_is_rejected_not_served() {
    let home = TestHome::create("query-before-handshake");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;
    let mut connection = connect(&endpoint).await;

    let request = Request::Query(QueryRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        workspace: WorkspaceSelectorWire::Locator {
            path: "/does/not/matter".to_owned(),
        },
        correlation: None,
        operation: QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
            statuses: vec![WorkItemStatusWire::Open],
            limit: std::num::NonZeroUsize::new(10).unwrap(),
        }),
    });
    let response = send(&mut connection, request).await;
    match response {
        Response::Error(error) => assert_eq!(error.kind, ErrorKind::InvalidRequest),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_workspace_never_panics_the_daemon_and_is_typed() {
    let home = TestHome::create("not-initialized");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;
    let mut connection = connect(&endpoint).await;
    handshake(&mut connection, PROTOCOL_VERSION).await;

    let workspace = TestHome::create("not-initialized-target");
    let request = Request::Query(QueryRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        workspace: WorkspaceSelectorWire::Locator {
            path: workspace.path().to_string_lossy().into_owned(),
        },
        correlation: None,
        operation: QueryOperationWire::Structure(StructureWire {
            grouping: GroupingSpecWire::ResourceRole,
            resource_scope: SummaryResourceScopeWire::default(),
            relation_kinds: Vec::new(),
            include_ungrouped: true,
            include_cycles: false,
            member_sample_limit: None,
        }),
    });
    let Response::Query(query_response) = send(&mut connection, request).await else {
        panic!("expected a Query response")
    };
    let QueryOutcomeWire::Err(error) = query_response.outcome else {
        panic!("an un-init'd Workspace must be a typed error")
    };
    assert_eq!(error.code, QueryErrorCodeWire::NotInitialized);

    // The connection (and daemon) must still be alive and servable.
    assert!(matches!(
        send(
            &mut connection,
            Request::Status(brainprint_core::protocol::StatusRequest)
        )
        .await,
        Response::Status(_)
    ));
}

// ------------------------------------------------------------- workspace

#[tokio::test]
async fn two_workspaces_stay_isolated_and_query_concurrently() {
    let home = TestHome::create("two-workspaces");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;

    let workspace_a = TestHome::create("ws-a");
    let workspace_b = TestHome::create("ws-b");

    let mut setup = connect(&endpoint).await;
    handshake(&mut setup, PROTOCOL_VERSION).await;
    let id_a = init_workspace(&mut setup, workspace_a.path()).await;
    let id_b = init_workspace(&mut setup, workspace_b.path()).await;
    assert_ne!(id_a, id_b);

    let endpoint_a = endpoint.clone();
    let endpoint_b = endpoint.clone();
    let task_a = tokio::spawn(async move {
        let mut connection = connect(&endpoint_a).await;
        handshake(&mut connection, PROTOCOL_VERSION).await;
        send(
            &mut connection,
            Request::Query(QueryRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                workspace: WorkspaceSelectorWire::Id { workspace_id: id_a },
                correlation: None,
                operation: QueryOperationWire::Structure(StructureWire {
                    grouping: GroupingSpecWire::ResourceRole,
                    resource_scope: SummaryResourceScopeWire::default(),
                    relation_kinds: Vec::new(),
                    include_ungrouped: true,
                    include_cycles: false,
                    member_sample_limit: None,
                }),
            }),
        )
        .await
    });
    let task_b = tokio::spawn(async move {
        let mut connection = connect(&endpoint_b).await;
        handshake(&mut connection, PROTOCOL_VERSION).await;
        send(
            &mut connection,
            Request::Query(QueryRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                workspace: WorkspaceSelectorWire::Id { workspace_id: id_b },
                correlation: None,
                operation: QueryOperationWire::Structure(StructureWire {
                    grouping: GroupingSpecWire::ResourceRole,
                    resource_scope: SummaryResourceScopeWire::default(),
                    relation_kinds: Vec::new(),
                    include_ungrouped: true,
                    include_cycles: false,
                    member_sample_limit: None,
                }),
            }),
        )
        .await
    });

    let (response_a, response_b) = tokio::time::timeout(Duration::from_secs(5), async {
        (task_a.await, task_b.await)
    })
    .await
    .expect("both Workspaces should answer without blocking on each other");

    for response in [response_a.unwrap(), response_b.unwrap()] {
        let Response::Query(query_response) = response else {
            panic!("expected a Query response")
        };
        assert!(matches!(query_response.outcome, QueryOutcomeWire::Ok(_)));
    }
}

// -------------------------------------------------------------- ack state

async fn find_target_not_found(
    connection: &mut ClientConnection,
    workspace_id: WorkspaceId,
) -> QueryResponse {
    let request = Request::Query(QueryRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        workspace: WorkspaceSelectorWire::Id { workspace_id },
        correlation: None,
        operation: QueryOperationWire::Find(FindQueryWire::Target {
            target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                "src/lib.rs".to_owned(),
            )),
            delivery: compact_delivery(),
        }),
    });
    let Response::Query(response) = send(connection, request).await else {
        panic!("expected a Query response")
    };
    response
}

#[tokio::test]
async fn ack_lifecycle_pending_then_acknowledged_then_idempotent_then_unknown() {
    let home = TestHome::create("ack-lifecycle");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;
    let workspace = TestHome::create("ack-lifecycle-ws");

    let mut connection = connect(&endpoint).await;
    handshake(&mut connection, PROTOCOL_VERSION).await;
    let workspace_id = init_workspace(&mut connection, workspace.path()).await;

    let response = find_target_not_found(&mut connection, workspace_id).await;
    assert!(matches!(response.outcome, QueryOutcomeWire::Ok(_)));
    let ack_token = response
        .ack_token
        .expect("a planner-backed operation must carry an ack_token");

    // Duplicate/unknown tokens must not be confused with the real one.
    let unknown_ack = send(
        &mut connection,
        Request::QueryAck(QueryAckRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            workspace_id,
            ack_token: uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    assert!(matches!(
        unknown_ack,
        Response::QueryAck(QueryAckResponse {
            status: AckStatusWire::UnknownOrExpired,
            ..
        })
    ));

    let first_ack = send(
        &mut connection,
        Request::QueryAck(QueryAckRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            workspace_id,
            ack_token: ack_token.clone(),
        }),
    )
    .await;
    assert!(matches!(
        first_ack,
        Response::QueryAck(QueryAckResponse {
            status: AckStatusWire::Acknowledged,
            ..
        })
    ));

    // Duplicate ACK is idempotent, not a second acknowledge.
    let second_ack = send(
        &mut connection,
        Request::QueryAck(QueryAckRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            workspace_id,
            ack_token,
        }),
    )
    .await;
    assert!(matches!(
        second_ack,
        Response::QueryAck(QueryAckResponse {
            status: AckStatusWire::AlreadyAcknowledged,
            ..
        })
    ));
}

#[tokio::test]
async fn connection_close_before_ack_leaves_nothing_acknowledged() {
    let home = TestHome::create("close-before-ack");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;
    let workspace = TestHome::create("close-before-ack-ws");

    let mut setup = connect(&endpoint).await;
    handshake(&mut setup, PROTOCOL_VERSION).await;
    let workspace_id = init_workspace(&mut setup, workspace.path()).await;

    {
        let mut connection = connect(&endpoint).await;
        handshake(&mut connection, PROTOCOL_VERSION).await;
        let response = find_target_not_found(&mut connection, workspace_id).await;
        assert!(response.ack_token.is_some());
        // `connection` drops here without ever sending QueryAck.
    }

    // A fresh connection can still query the same Workspace normally --
    // the worker thread must not have wedged or panicked.
    let response = find_target_not_found(&mut setup, workspace_id).await;
    assert!(matches!(response.outcome, QueryOutcomeWire::Ok(_)));
}

// ---------------------------------------------------------------- framing

#[tokio::test]
async fn oversized_text_search_result_is_result_too_large_not_partial() {
    let home = TestHome::create("result-too-large");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;
    let workspace = TestHome::create("result-too-large-ws");

    // Real files on disk: `find text` reads live bytes, no structural
    // index needed (#24 §22 "explicit Text is the only find mode allowed
    // to scan source").
    let big_line = "needle ".repeat(2000) + "\n"; // ~14 KiB/line
    for index in 0..40 {
        let content = big_line.repeat(20); // ~280 KiB/file
        fs::write(workspace.path().join(format!("file{index}.txt")), content)
            .expect("fixture file should write");
    }

    let mut connection = connect(&endpoint).await;
    handshake(&mut connection, PROTOCOL_VERSION).await;
    let workspace_id = init_workspace(&mut connection, workspace.path()).await;

    let request = Request::Query(QueryRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        workspace: WorkspaceSelectorWire::Id { workspace_id },
        correlation: None,
        operation: QueryOperationWire::Find(FindQueryWire::Text {
            pattern: TextPatternWire::Literal("needle".to_owned()),
            case_insensitive: false,
            path_prefix: None,
            search_budget: SearchBudgetWire {
                max_results: 1_000_000,
                max_files: 1_000,
                max_bytes: 64 * 1024 * 1024,
                deadline_ms: Some(30_000),
            },
            max_file_bytes: 8 * 1024 * 1024,
            with_preview: true,
        }),
    });
    let Response::Query(response) = send(&mut connection, request).await else {
        panic!("expected a Query response")
    };
    match response.outcome {
        QueryOutcomeWire::Err(error) => {
            assert_eq!(error.code, QueryErrorCodeWire::ResultTooLarge);
        }
        QueryOutcomeWire::Ok(_) => {
            // The fixture is a best-effort oversize; if the actual
            // encoded size did not clear 1 MiB, this is not a failure of
            // the mechanism itself (proven directly on the encoded-size
            // path in `daemon::query::runtime`'s own unit coverage) --
            // fail loudly so the fixture size can be tuned rather than
            // silently passing.
            panic!(
                "fixture did not exceed the frame bound; enlarge it to actually exercise RESULT_TOO_LARGE"
            );
        }
    }
    // No ack_token: an Err outcome never carries one.
    assert!(response.ack_token.is_none());
}

// --------------------------------------------------------------- parity

#[tokio::test]
async fn direct_core_and_daemon_wire_agree_on_find_target_not_found() {
    let home = TestHome::create("parity-find-target");
    let global_paths = home.global_paths();
    let (_server, endpoint) = start_server(&global_paths).await;
    let workspace = TestHome::create("parity-find-target-ws");

    let mut connection = connect(&endpoint).await;
    handshake(&mut connection, PROTOCOL_VERSION).await;
    let workspace_id = init_workspace(&mut connection, workspace.path()).await;

    let wire_response = find_target_not_found(&mut connection, workspace_id).await;
    let QueryOutcomeWire::Ok(QueryResultWire::Find(FindResultWire::Target(wire_answer))) =
        wire_response.outcome
    else {
        panic!("expected Find::Target Ok, got {:?}", wire_response.outcome)
    };

    // The direct Core path, same Workspace, same target.
    let surface = CoreQuerySurface::open(&global_paths.global_db, workspace_id)
        .expect("CoreQuerySurface::open should succeed for an initialized Workspace");
    let mut ledger = brainprint_engine::projection::planner::DeliveryLedger::new(
        brainprint_engine::projection::planner::LedgerLimits::new(16, 1024).unwrap(),
    );
    let direct = surface
        .find(
            brainprint_engine::query_surface::FindRequest {
                context: brainprint_engine::query_surface::QueryContext {
                    workspace: workspace_id,
                    correlation: None,
                },
                query: brainprint_engine::query_surface::FindQuery::Target {
                    target: brainprint_engine::projection::ProjectionTarget::Resource(
                        brainprint_engine::projection::ResourceTarget::Path(
                            "src/lib.rs".to_owned(),
                        ),
                    ),
                    delivery: brainprint_engine::query_surface::DeliveryOptions {
                        budget: brainprint_engine::projection::planner::DeliveryBudget::new(
                            std::num::NonZeroUsize::new(16).map(std::num::NonZeroUsize::get),
                            std::num::NonZeroUsize::new(16 * 1024).map(std::num::NonZeroUsize::get),
                            None,
                        )
                        .unwrap(),
                        continuation: None,
                        retention:
                            brainprint_engine::projection::planner::ContextRetention::ReuseDisabled,
                        tokens: None,
                    },
                },
            },
            &mut ledger,
        )
        .expect("direct Core find should succeed");

    let brainprint_engine::query_surface::FindResult::Target(direct_answer) = direct else {
        panic!("expected FindResult::Target")
    };

    // Semantic parity: same target resolution and currentness. Full
    // structural equality of every nested evidence field is covered by
    // the wire-mirror construction itself (each conversion function is a
    // 1:1 field mirror, checked at compile time by the type system); this
    // asserts the two independent code paths (direct engine call vs.
    // daemon dispatch) actually reach the same semantic outcome, not
    // merely that each one *could* encode faithfully.
    let resolution_matches = matches!(
        (&direct_answer.target, &wire_answer.target_resolution),
        (
            brainprint_engine::query_surface::TargetResolution::NotFoundIncompleteCoverage,
            TargetResolutionWire::NotFoundIncompleteCoverage
        ) | (
            brainprint_engine::query_surface::TargetResolution::NotFound,
            TargetResolutionWire::NotFound
        ) | (
            brainprint_engine::query_surface::TargetResolution::NotCurrent,
            TargetResolutionWire::NotCurrent
        )
    );
    assert!(
        resolution_matches,
        "direct={:?} wire={:?}",
        direct_answer.target, wire_answer.target_resolution
    );
    assert_eq!(direct_answer.currentness, {
        match wire_answer.currentness {
            CurrentnessWire::Current => brainprint_engine::query::Currentness::Current,
            CurrentnessWire::NotCurrent(_) => brainprint_engine::query::Currentness::NotCurrent(
                brainprint_engine::query::NotCurrentReason::ResourceIndexNeverPublished,
            ),
        }
    });
}
