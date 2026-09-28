//! #39 (P0 correction) semantic runtime product integration acceptance.
//!
//! Every Workspace here is produced and queried through the product
//! path: a real daemon `Server`, `Init` and `Query` over the real local
//! IPC, the daemon-owned Workspace runtime and its lazily activated
//! semantic supervisor, and the real backends located from a real
//! Brainprint `config.toml` in an isolated home. No test hands a launcher
//! an install root directly; the only direct engine call is the Task 10
//! `CoreQuerySurface` itself (acceptance 12).
//!
//! The real-backend cases skip with a printed reason when the pinned
//! install is absent (`scripts/*_semantic_spike`, see #19); #39 cannot be
//! marked complete from a run that skipped them.

use std::{
    env, fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use brainprint_core::{
    PROTOCOL_VERSION, WorkspaceId,
    protocol::{
        self, ClientConnection, HandshakeRequest, InitRequest, InitResponse, Request, Response,
        query::*,
    },
};
use brainprint_daemon::{
    query::{DaemonQueryRuntime, semantic::SemanticStats},
    runtime_paths,
    server::Server,
};
use brainprint_engine::{
    paths::{GlobalPaths, WorkspacePaths},
    projection::{ProjectionTarget, SymbolName, SymbolTarget},
    query_surface::{
        CoreQuerySurface, QueryContext, RelationDirection, RelationsRequest, TargetResolution,
    },
    resource::ResourceLanguage,
    semantic_index::{SemanticIndex, SemanticState},
};
use brainprint_mcp::BrainprintMcp;
use rmcp::handler::server::wrapper::Parameters;
use serde_json::json;

// ------------------------------------------------------------- harness

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "bp-p039-{label}-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test dir");
        Self(path.canonicalize().expect("canonical"))
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

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn spike(name: &str) -> PathBuf {
    repo()
        .join("scripts")
        .join(name)
        .canonicalize()
        .expect("spike dir")
}

/// Whether the pinned install a real-backend case needs is present.
fn installed(name: &str, package: &str) -> bool {
    let present = repo()
        .join("scripts")
        .join(name)
        .join("node_modules")
        .join(package)
        .is_dir();
    if !present {
        println!("skipped: no pinned {package} under scripts/{name}");
    }
    present
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// An isolated Brainprint home plus one copied fixture Workspace.
struct Home {
    dir: TestDir,
    global: GlobalPaths,
}

impl Home {
    fn new(label: &str) -> Self {
        let dir = TestDir::create(label);
        let global = GlobalPaths::from_home(dir.path());
        fs::create_dir_all(&global.root).expect("home root");
        Self { dir, global }
    }

    /// Write the user's real global `config.toml` (#39 locator contract).
    fn config(&self, body: &str) {
        fs::write(
            &self.global.config_file,
            format!("format_version = 1\n{body}"),
        )
        .expect("config");
    }

    fn python_config(&self, install_root: &Path) {
        self.config(&format!(
            "\n[semantic_backends.python]\ninstall_root = \"{}\"\n",
            install_root.display()
        ));
    }

    fn workspace(&self, name: &str, fixture: &str) -> PathBuf {
        let root = self.dir.path().join(name);
        copy_tree(&repo().join("fixtures/workspaces").join(fixture), &root);
        root
    }

    /// A per-test alias of the pinned Pyright install, so this test's
    /// backend processes are identifiable by command line.
    fn python_install_alias(&self) -> PathBuf {
        let alias = self.dir.path().join("pyright-install");
        #[cfg(unix)]
        std::os::unix::fs::symlink(spike("python_semantic_spike"), &alias).expect("alias");
        alias
    }
}

struct Daemon {
    handle: tokio::task::JoinHandle<()>,
    endpoint: runtime_paths::RuntimeEndpoint,
    runtime: Arc<DaemonQueryRuntime>,
}

impl Daemon {
    async fn start(global: &GlobalPaths) -> Self {
        let mut server = Server::bind(global).await.expect("bind");
        let runtime = server.query_runtime();
        let endpoint = runtime_paths::resolve(global);
        let handle = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        Self {
            handle,
            endpoint,
            runtime,
        }
    }

    async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
        drop(self.runtime);
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    async fn connect(&self) -> ClientConnection {
        #[cfg(unix)]
        let connection = ClientConnection::connect(&self.endpoint.socket_path).await;
        #[cfg(windows)]
        let connection = ClientConnection::connect(&self.endpoint.pipe_name).await;
        let mut connection = connection.expect("connect");
        send(
            &mut connection,
            Request::Handshake(HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_kind: "p0-39".to_owned(),
            }),
        )
        .await;
        connection
    }

    async fn init(&self, root: &Path) -> WorkspaceId {
        let mut connection = self.connect().await;
        match send(
            &mut connection,
            Request::Init(InitRequest {
                path: root.to_string_lossy().into_owned(),
            }),
        )
        .await
        {
            Response::Init(InitResponse { workspace_id, .. }) => workspace_id.parse().expect("id"),
            other => panic!("init failed: {other:?}"),
        }
    }

    async fn semantic(&self, workspace: WorkspaceId) -> SemanticStats {
        self.runtime
            .semantic_stats(workspace)
            .await
            .expect("workspace semantic runtime exists")
    }

    async fn query(
        &self,
        workspace: WorkspaceId,
        operation: QueryOperationWire,
    ) -> QueryResultWire {
        query(&mut self.connect().await, workspace, operation).await
    }

    async fn relations(
        &self,
        workspace: WorkspaceId,
        target: ProjectionTargetWire,
        direction: RelationDirectionWire,
    ) -> RelationsResultWire {
        match self
            .query(
                workspace,
                QueryOperationWire::Relations(RelationsWire {
                    target,
                    direction,
                    kinds: Vec::new(),
                }),
            )
            .await
        {
            QueryResultWire::Relations(result) => result,
            other => panic!("expected relations: {other:?}"),
        }
    }

    async fn outgoing(&self, workspace: WorkspaceId, name: &str) -> RelationAnswerWire {
        let result = self
            .relations(
                workspace,
                symbol(name, ResourceLanguageWire::Python),
                RelationDirectionWire::Outgoing,
            )
            .await;
        assert!(
            matches!(result.target, TargetResolutionWire::Resolved(_)),
            "{name} resolves: {:?}",
            result.target
        );
        assert_eq!(result.currentness, CurrentnessWire::Current);
        result.answers.into_iter().next().expect("one answer")
    }
}

async fn send(connection: &mut ClientConnection, request: Request) -> Response {
    protocol::framing::write_message(connection, &request)
        .await
        .expect("write");
    protocol::framing::read_message(connection)
        .await
        .expect("read")
}

async fn query(
    connection: &mut ClientConnection,
    workspace: WorkspaceId,
    operation: QueryOperationWire,
) -> QueryResultWire {
    let Response::Query(response) = send(
        connection,
        Request::Query(QueryRequest {
            request_id: "0d0f6c1e-39a0-4c2b-9f39-000000000039".to_owned(),
            workspace: WorkspaceSelectorWire::Id {
                workspace_id: workspace,
            },
            correlation: None,
            operation,
        }),
    )
    .await
    else {
        panic!("expected a Query response")
    };
    match response.outcome {
        QueryOutcomeWire::Ok(result) => result,
        QueryOutcomeWire::Err(error) => panic!("query error: {error:?}"),
    }
}

fn symbol(name: &str, language: ResourceLanguageWire) -> ProjectionTargetWire {
    ProjectionTargetWire::Symbol(SymbolTargetWire {
        name: SymbolNameWire::QualifiedName(name.to_owned()),
        resource: None,
        kind: None,
        language: Some(language),
    })
}

fn delivery() -> DeliveryWire {
    DeliveryWire {
        budget: DeliveryBudgetWire {
            max_items: NonZeroUsize::new(64),
            max_bytes: NonZeroUsize::new(64 * 1024),
        },
        continuation: None,
        retention: RetentionWire::Disabled,
    }
}

fn find_files() -> QueryOperationWire {
    QueryOperationWire::Find(FindQueryWire::Files {
        directory: None,
        recursive: true,
        path_prefix: None,
        role: None,
        language: None,
        kind: Some(ResourceKindWire::File),
        limit: NonZeroUsize::new(200).expect("nz"),
    })
}

fn calls(answer: &RelationAnswerWire) -> usize {
    answer
        .confirmed
        .iter()
        .filter(|relation| relation.kind == RelationKindWire::Calls)
        .count()
}

fn receiver_gaps(answer: &RelationAnswerWire) -> usize {
    answer
        .gaps
        .iter()
        .filter(|gap| gap.reason == UnresolvedReasonWire::ReceiverTypeRequired)
        .count()
}

/// Level B truth for `call`: the receiver-typed call is an explicit
/// gap, never a false zero.
fn assert_structural_gap(answer: &RelationAnswerWire) {
    assert_eq!(calls(answer), 0, "no CALLS edge without semantics");
    assert_eq!(receiver_gaps(answer), 1, "the call site stays a gap");
    assert!(
        answer.coverage.requires_semantics >= 1,
        "coverage says semantics would be needed: {:?}",
        answer.coverage
    );
}

/// Level A truth for `call`: one CALLS edge, the gap closed, semantic
/// contribution current.
fn assert_enriched(answer: &RelationAnswerWire) {
    assert_eq!(calls(answer), 1, "x.run(1) binds: {:?}", answer.confirmed);
    assert_eq!(receiver_gaps(answer), 0, "{:?}", answer.gaps);
    assert_eq!(answer.coverage.requires_semantics, 0);
    assert_eq!(answer.coverage.semantic.contexts, 1);
    assert!(!answer.coverage.semantic.not_current);
}

/// Semantic owner states persisted in one Workspace's index.
fn owner_states(root: &Path) -> Vec<SemanticState> {
    let index = SemanticIndex::open(&WorkspacePaths::from_root(root).index_db).expect("index");
    let contexts: Vec<String> = index
        .connection()
        .prepare("SELECT DISTINCT context_key FROM semantic_publication")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    let mut states = Vec::new();
    for context in contexts {
        for owner in index.owners_of_context(&context).expect("owners") {
            states.push(index.status(&owner).expect("status").state);
        }
    }
    states
}

fn processes_matching(pattern: &str) -> usize {
    Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).lines().count())
        .unwrap_or(0)
}

fn append(path: &Path, text: &str) {
    let mut body = fs::read_to_string(path).expect("read");
    body.push_str(text);
    fs::write(path, body).expect("write");
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

// ---------------------------------------------------------- acceptance

/// 2, 3, 18: registration is not a start; deterministic-first queries
/// never wake the backend.
#[tokio::test]
async fn fresh_init_and_structural_queries_start_nothing() {
    if !installed("python_semantic_spike", "pyright-typeserver") {
        return;
    }
    let home = Home::new("deterministic");
    let alias = home.python_install_alias();
    home.python_config(&alias);
    let root = home.workspace("ws", "python-semantic-spike");
    let daemon = Daemon::start(&home.global).await;
    let workspace = daemon.init(&root).await;

    let stats = daemon.semantic(workspace).await;
    assert_eq!(stats.registered, vec!["python"]);
    assert_eq!(stats.backend_starts, 0, "fresh init starts nothing");
    assert_eq!(stats.live_runtimes, 0);

    // find files, find target, knowledge, structure, and an inspect /
    // relations whose structural evidence is complete.
    daemon.query(workspace, find_files()).await;
    daemon
        .query(
            workspace,
            QueryOperationWire::Find(FindQueryWire::Target {
                target: symbol("call", ResourceLanguageWire::Python),
                delivery: delivery(),
            }),
        )
        .await;
    daemon
        .query(
            workspace,
            QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
                statuses: vec![WorkItemStatusWire::Open],
                limit: NonZeroUsize::new(10).expect("nz"),
            }),
        )
        .await;
    daemon
        .query(
            workspace,
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
    daemon
        .query(
            workspace,
            QueryOperationWire::Inspect(InspectWire {
                target: symbol("Base.run", ResourceLanguageWire::Python),
                delivery: delivery(),
            }),
        )
        .await;
    daemon
        .relations(
            workspace,
            symbol("Other.run", ResourceLanguageWire::Python),
            RelationDirectionWire::Both,
        )
        .await;

    let stats = daemon.semantic(workspace).await;
    assert_eq!(stats.backend_starts, 0, "{stats:?}");
    assert_eq!(stats.backend_requests, 0, "{stats:?}");
    assert_eq!(stats.live_runtimes, 0);
    assert_eq!(stats.demands, 0);
    assert!(stats.probes >= 2, "the two target queries were probed");
    assert_eq!(processes_matching(&alias.to_string_lossy()), 0);
    daemon.stop().await;
}

/// 4, 5, 6, 12, 13, 14, 16, 18: a real REQUIRES_SEMANTICS gap drives the
/// shared lazy backend through every P0 operation, CLI-wire and MCP.
#[tokio::test]
async fn python_gaps_lazily_enrich_through_every_product_operation() {
    if !installed("python_semantic_spike", "pyright-typeserver") {
        return;
    }
    let home = Home::new("python");
    let alias = home.python_install_alias();
    home.python_config(&alias);
    let root = home.workspace("ws", "python-semantic-spike");
    let daemon = Daemon::start(&home.global).await;
    let workspace = daemon.init(&root).await;

    // 12: Task 10's CoreQuerySurface, called directly, owns no backend:
    // it answers structurally with the explicit gap and starts nothing.
    let surface = CoreQuerySurface::open(&home.global.global_db, workspace).expect("surface");
    let direct = surface
        .relations(RelationsRequest {
            context: QueryContext {
                workspace,
                correlation: None,
            },
            target: ProjectionTarget::Symbol(SymbolTarget {
                language: Some(ResourceLanguage::Python),
                ..SymbolTarget::new(SymbolName::QualifiedName("call".to_owned()))
            }),
            direction: RelationDirection::Outgoing,
            kinds: Vec::new(),
        })
        .expect("direct relations");
    assert!(matches!(direct.target, TargetResolution::Resolved(_)));
    assert_eq!(direct.answers[0].coverage.requires_semantics, 1);
    assert_eq!(processes_matching(&alias.to_string_lossy()), 0);
    drop(surface);

    // 4/13: the product query (the CLI's own wire request) enriches.
    assert_enriched(&daemon.outgoing(workspace, "call").await);
    let stats = daemon.semantic(workspace).await;
    assert_eq!(stats.backend_starts, 1, "{stats:?}");
    assert_eq!(stats.backend_start_successes, 1);
    assert_eq!(stats.live_runtimes, 1);
    assert_eq!(stats.owners_refreshed, 1, "only the selected owner");
    assert!(stats.backend_requests > 0);
    assert_eq!(processes_matching(&alias.to_string_lossy()), 1);

    // 6: warm reuse -- the answer is current, nothing is re-requested.
    let requests = stats.backend_requests;
    assert_enriched(&daemon.outgoing(workspace, "call").await);
    let stats = daemon.semantic(workspace).await;
    assert_eq!(stats.backend_starts, 1);
    assert_eq!(stats.backend_requests, requests, "current: no request");

    // 5: two concurrent clients, three more owners through inspect,
    // impact and context(Change): one runtime, one process.
    let inspect = daemon.query(
        workspace,
        QueryOperationWire::Inspect(InspectWire {
            target: symbol("Qualified", ResourceLanguageWire::Python),
            delivery: delivery(),
        }),
    );
    let impact = daemon.query(
        workspace,
        QueryOperationWire::Impact(ImpactWire {
            target: symbol("Abstract", ResourceLanguageWire::Python),
            change: ChangeKindWire::Structural(ImpactIntentWire::PublicSignatureChange),
            delivery: delivery(),
        }),
    );
    let (inspected, impacted) = tokio::join!(inspect, impact);
    assert!(matches!(inspected, QueryResultWire::Inspect(_)));
    assert!(matches!(impacted, QueryResultWire::Impact(_)));
    let context = daemon
        .query(
            workspace,
            QueryOperationWire::Context(ContextWire::Change {
                target: symbol("호출", ResourceLanguageWire::Python),
                change: None,
                work_item: None,
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge_refs: ProjectionKnowledgeRefsWire::default(),
                delivery: delivery(),
            }),
        )
        .await;
    assert!(matches!(context, QueryResultWire::Context(_)));
    let stats = daemon.semantic(workspace).await;
    assert_eq!(stats.backend_starts, 1, "shared runtime: {stats:?}");
    assert_eq!(stats.live_runtimes, 1);
    assert_eq!(
        stats.owners_refreshed, 4,
        "impl, inherit, shapes, unicode_case"
    );
    assert_eq!(stats.owner_failures, 0);
    assert_eq!(processes_matching(&alias.to_string_lossy()), 1);

    // The enrichment those operations triggered is canonical truth.
    let qualified = daemon.outgoing(workspace, "호출").await;
    assert_eq!(calls(&qualified), 1, "{:?}", qualified.confirmed);

    // 14: the MCP product tool reaches the same runtime and truth.
    let mcp = BrainprintMcp::with_endpoint(
        root.clone(),
        brainprint_core::protocol::EndpointPaths::from_runtime_root(home.global.runtime_root()),
    );
    let result = mcp
        .relations(Parameters(
            serde_json::from_value(json!({
                "workspace_path": root.to_string_lossy(),
                "mode": "direct",
                "qualified_symbol_name": "call",
                "symbol_language": "Python",
                "direction": "outgoing",
            }))
            .expect("params"),
        ))
        .await
        .expect("brainprint.relations");
    let payload: QueryResultWire =
        serde_json::from_value(result.structured_content.expect("structured")["payload"].clone())
            .expect("payload");
    let QueryResultWire::Relations(via_mcp) = payload else {
        panic!("relations payload")
    };
    assert_enriched(&via_mcp.answers[0]);
    let stats = daemon.semantic(workspace).await;
    assert_eq!(stats.backend_starts, 1, "MCP shares the runtime");
    daemon.stop().await;
}

/// 7, 11: a source edit invalidates the prior semantic basis; the
/// structural answer does not wait for semantics; the next semantic
/// query refreshes on the warm process.
#[tokio::test]
async fn source_edit_invalidates_then_next_semantic_query_refreshes() {
    if !installed("python_semantic_spike", "pyright-typeserver") {
        return;
    }
    let home = Home::new("mutation");
    let alias = home.python_install_alias();
    home.python_config(&alias);
    let root = home.workspace("ws", "python-semantic-spike");
    let daemon = Daemon::start(&home.global).await;
    let workspace = daemon.init(&root).await;

    assert_enriched(&daemon.outgoing(workspace, "call").await);
    assert_eq!(owner_states(&root), vec![SemanticState::Current]);
    let before = daemon.semantic(workspace).await;

    append(
        &root.join("pkg/impl.py"),
        "\n\ndef call_again(y: Base):\n    return y.run(2)\n",
    );
    // Structural truth moves on by itself; no backend work is waited for.
    eventually("call_again is indexed", || async {
        let QueryResultWire::Find(FindResultWire::Target(answer)) = daemon
            .query(
                workspace,
                QueryOperationWire::Find(FindQueryWire::Target {
                    target: symbol("call_again", ResourceLanguageWire::Python),
                    delivery: delivery(),
                }),
            )
            .await
        else {
            return false;
        };
        matches!(answer.target_resolution, TargetResolutionWire::Resolved(_))
    })
    .await;
    // 11: the old semantic publication is not current for the new source.
    assert_eq!(owner_states(&root), vec![SemanticState::Dirty]);
    let after_structural = daemon.semantic(workspace).await;
    assert_eq!(after_structural.backend_requests, before.backend_requests);
    assert_eq!(after_structural.owners_refreshed, before.owners_refreshed);

    // 7: the next semantic-required query refreshes, on the warm process.
    let answer = daemon.outgoing(workspace, "call_again").await;
    assert_eq!(calls(&answer), 1, "{answer:?}");
    assert_eq!(receiver_gaps(&answer), 0);
    assert_enriched(&daemon.outgoing(workspace, "call").await);
    assert_eq!(owner_states(&root), vec![SemanticState::Current]);
    let stats = daemon.semantic(workspace).await;
    assert_eq!(stats.backend_starts, 1, "no respawn for an edit: {stats:?}");
    assert_eq!(stats.change_notifications, 1, "the edit was delivered");
    assert_eq!(processes_matching(&alias.to_string_lossy()), 1);
    daemon.stop().await;
}

/// 8: no usable backend -> structural Level B/C with an explicit gap,
/// with the exact reason recorded, and nothing started.
#[tokio::test]
async fn unavailable_backends_keep_structural_truth_and_explicit_gaps() {
    for (label, config) in [
        ("none", String::new()),
        (
            "relative",
            "\n[semantic_backends.python]\ninstall_root = \"scripts/python_semantic_spike\"\n"
                .to_owned(),
        ),
        (
            "missing",
            "\n[semantic_backends.python]\ninstall_root = \"/nonexistent/pyright\"\n".to_owned(),
        ),
    ] {
        let home = Home::new(label);
        home.config(&config);
        let root = home.workspace("ws", "python-semantic-spike");
        let daemon = Daemon::start(&home.global).await;
        let workspace = daemon.init(&root).await;

        assert_structural_gap(&daemon.outgoing(workspace, "call").await);
        let stats = daemon.semantic(workspace).await;
        assert!(stats.registered.is_empty(), "{label}: {stats:?}");
        let reason = &stats.unavailable["python"];
        match label {
            "none" => assert!(reason.contains("no backend locator"), "{reason}"),
            "relative" => assert!(reason.contains("not an absolute path"), "{reason}"),
            _ => assert!(reason.contains("/nonexistent/pyright"), "{reason}"),
        }
        assert!(stats.unavailable["csharp"].contains("no backend locator"));
        assert_eq!(stats.backend_starts, 0);
        assert!(owner_states(&root).is_empty(), "nothing published");
        daemon.stop().await;
    }

    // C# / Rust locators are recorded, never started: Level A loads the
    // project, and this Workspace's config grants no trust.
    let home = Home::new("trust");
    home.config(
        "\n[semantic_backends.csharp]\ninstall_root = \"/opt/roslyn\"\n\n[semantic_backends.rust]\nexecutable = \"/opt/rust-analyzer\"\n",
    );
    let root = home.workspace("ws", "python-semantic-spike");
    let daemon = Daemon::start(&home.global).await;
    let workspace = daemon.init(&root).await;
    let stats = daemon.semantic(workspace).await;
    for family in ["csharp", "rust"] {
        assert!(
            stats.unavailable[family].contains("ProjectExecutionTrust::Trusted"),
            "{family}: {stats:?}"
        );
    }
    daemon.stop().await;
}

/// 9: a crashed backend and a backend that cannot start: the daemon
/// survives, answers structurally with the gap, never serves a stale
/// semantic answer as current, and restarts only within its budget.
#[tokio::test]
async fn crash_and_backoff_keep_the_daemon_and_never_serve_stale() {
    if !installed("python_semantic_spike", "pyright-typeserver") {
        return;
    }
    // --- a real backend that dies under the daemon
    let home = Home::new("crash");
    let alias = home.python_install_alias();
    home.python_config(&alias);
    let root = home.workspace("ws", "python-semantic-spike");
    let daemon = Daemon::start(&home.global).await;
    let workspace = daemon.init(&root).await;
    assert_enriched(&daemon.outgoing(workspace, "call").await);

    let killed = Command::new("pkill")
        .args(["-9", "-f", &alias.to_string_lossy()])
        .status()
        .expect("pkill");
    assert!(killed.success());
    eventually("the backend is gone", || async {
        processes_matching(&alias.to_string_lossy()) == 0
    })
    .await;
    append(
        &root.join("pkg/impl.py"),
        "\n\ndef call_again(y: Base):\n    return y.run(2)\n",
    );
    eventually("call_again is indexed", || async {
        daemon
            .relations(
                workspace,
                symbol("call_again", ResourceLanguageWire::Python),
                RelationDirectionWire::Outgoing,
            )
            .await
            .target
            != TargetResolutionWire::NotFound
    })
    .await;
    // Whatever the first post-crash answer is, it is never stale-as-
    // current: either freshly enriched, or the gap with coverage saying so.
    for _ in 0..4 {
        let answer = daemon.outgoing(workspace, "call_again").await;
        if calls(&answer) == 1 {
            assert_eq!(receiver_gaps(&answer), 0);
            assert!(!answer.coverage.semantic.not_current);
            break;
        }
        assert_eq!(receiver_gaps(&answer), 1, "{answer:?}");
        assert!(answer.coverage.requires_semantics >= 1);
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    assert_eq!(calls(&daemon.outgoing(workspace, "call_again").await), 1);
    let stats = daemon.semantic(workspace).await;
    assert!(stats.backend_crashes >= 1, "{stats:?}");
    assert_eq!(stats.backend_start_successes, 2, "one bounded restart");
    daemon.stop().await;

    // --- a backend that never starts: bounded attempts, then degraded
    let home = Home::new("backoff");
    let broken = home.dir.path().join("broken-pyright");
    let package = broken.join("node_modules/pyright-typeserver");
    fs::create_dir_all(&package).expect("package");
    fs::write(
        package.join("package.json"),
        "{\"version\":\"0.0.0-broken\"}",
    )
    .expect("manifest");
    fs::write(package.join("pyright-typeserver.js"), "process.exit(3);\n").expect("script");
    home.python_config(&broken);
    let root = home.workspace("ws", "python-semantic-spike");
    let daemon = Daemon::start(&home.global).await;
    let workspace = daemon.init(&root).await;
    for _ in 0..8 {
        assert_structural_gap(&daemon.outgoing(workspace, "call").await);
    }
    let stats = daemon.semantic(workspace).await;
    assert_eq!(stats.registered, vec!["python"]);
    assert_eq!(stats.backend_start_successes, 0);
    assert!(
        (1..=3).contains(&stats.backend_starts),
        "restarts are bounded by the budget/backoff: {stats:?}"
    );
    assert_eq!(stats.start_failures, 8, "every demand fell back: {stats:?}");
    assert!(owner_states(&root).is_empty(), "nothing published");
    // The daemon is still serving.
    daemon.query(workspace, find_files()).await;
    daemon.stop().await;
}

/// 10: two worktrees are two Workspaces: separate runtimes, separate
/// processes, separate semantic currentness.
#[tokio::test]
async fn worktree_semantic_runtimes_are_isolated() {
    if !installed("python_semantic_spike", "pyright-typeserver") {
        return;
    }
    let home = Home::new("worktrees");
    let alias = home.python_install_alias();
    home.python_config(&alias);
    let root_a = home.workspace("a", "python-semantic-spike");
    let root_b = home.workspace("b", "python-semantic-spike");
    let daemon = Daemon::start(&home.global).await;
    let a = daemon.init(&root_a).await;
    let b = daemon.init(&root_b).await;
    assert_ne!(a, b);

    assert_enriched(&daemon.outgoing(a, "call").await);
    daemon.query(b, find_files()).await;
    assert_eq!(daemon.semantic(a).await.live_runtimes, 1);
    assert_eq!(daemon.semantic(b).await.backend_starts, 0, "B never asked");
    assert!(owner_states(&root_b).is_empty(), "A's result is not B's");

    assert_enriched(&daemon.outgoing(b, "call").await);
    assert_eq!(daemon.semantic(b).await.backend_starts, 1);
    assert_eq!(processes_matching(&alias.to_string_lossy()), 2);

    // An edit in A dirties A only.
    append(&root_a.join("pkg/impl.py"), "\n# edited in A\n");
    eventually("A's edit is published", || async {
        daemon.query(a, find_files()).await;
        owner_states(&root_a) == vec![SemanticState::Dirty]
    })
    .await;
    assert_eq!(owner_states(&root_b), vec![SemanticState::Current]);
    let requests = daemon.semantic(b).await.backend_requests;
    assert_enriched(&daemon.outgoing(b, "call").await);
    assert_eq!(daemon.semantic(b).await.backend_requests, requests);
    daemon.stop().await;
}

/// The real C# / Rust installs, or the printed reason a case skips.
fn csharp_install() -> Option<PathBuf> {
    let root = repo().join("scripts/csharp_semantic_spike");
    if root.join("packages").is_dir() {
        Some(root.canonicalize().expect("canonical"))
    } else {
        println!("skipped: no pinned Roslyn under scripts/csharp_semantic_spike");
        None
    }
}

fn rust_analyzer() -> Option<PathBuf> {
    let which = Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output()
        .ok()
        .filter(|output| output.status.success());
    let Some(which) = which else {
        println!("skipped: rust-analyzer is not installed");
        return None;
    };
    Some(PathBuf::from(String::from_utf8_lossy(&which.stdout).trim()))
}

/// The Workspace's own config: the only owner of the trust decision.
fn workspace_config(root: &Path, body: &str) {
    let paths = WorkspacePaths::from_root(root);
    fs::create_dir_all(&paths.root).expect("workspace config root");
    fs::write(&paths.config_file, format!("format_version = 1\n{body}")).expect("config");
}

/// A symbol target, pinned to one Resource when `within` is given.
async fn target_for(
    daemon: &Daemon,
    workspace: WorkspaceId,
    name: &str,
    language: ResourceLanguageWire,
    within: Option<&str>,
) -> ProjectionTargetWire {
    let mut target = symbol(name, language);
    if let (Some(path), ProjectionTargetWire::Symbol(symbol)) = (within, &mut target) {
        let resolved = daemon
            .relations(
                workspace,
                ProjectionTargetWire::Resource(ResourceTargetWire::Path(path.to_owned())),
                RelationDirectionWire::Outgoing,
            )
            .await
            .target;
        let TargetResolutionWire::Resolved(GraphEndpointWire::Resource(resource)) = resolved else {
            panic!("{path}: {resolved:?}")
        };
        symbol.resource = Some(resource);
    }
    target
}

async fn outgoing_within(
    daemon: &Daemon,
    workspace: WorkspaceId,
    name: &str,
    language: ResourceLanguageWire,
    within: Option<&str>,
) -> RelationAnswerWire {
    let target = target_for(daemon, workspace, name, language, within).await;
    let result = daemon
        .relations(workspace, target.clone(), RelationDirectionWire::Outgoing)
        .await;
    assert!(
        matches!(result.target, TargetResolutionWire::Resolved(_)),
        "{target:?}: {:?}",
        result.target
    );
    assert_eq!(result.currentness, CurrentnessWire::Current);
    result.answers.into_iter().next().expect("one answer")
}

/// Call sites, not distinct callees: an edit may call a known target again.
fn call_sites(answer: &RelationAnswerWire) -> usize {
    answer
        .confirmed
        .iter()
        .filter(|relation| relation.kind == RelationKindWire::Calls)
        .map(|relation| relation.evidence.len())
        .sum()
}

fn semantic_gaps(answer: &RelationAnswerWire) -> usize {
    answer.coverage.requires_semantics
}

/// 17 / trust correction: C# and Rust Level A is off until the
/// Workspace's own config says `Trusted`; then a real gap lazily starts
/// the real backend through the product path, and structural-only
/// queries still start nothing.
#[tokio::test]
async fn csharp_and_rust_enrich_only_under_explicit_workspace_trust() {
    struct Case {
        family: &'static str,
        fixture: &'static str,
        name: &'static str,
        language: ResourceLanguageWire,
        /// Narrows an otherwise ambiguous name to one Resource.
        within: Option<&'static str>,
        /// One more call the target makes after an edit: (file, anchor,
        /// inserted after the anchor).
        edit: (&'static str, &'static str, &'static str),
        locator: String,
        alias: PathBuf,
    }
    let mut cases = Vec::new();
    let home = Home::new("trusted");
    #[cfg_attr(not(unix), allow(unused_variables))]
    if let Some(install) = csharp_install() {
        let alias = home.dir.path().join("roslyn-install");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&install, &alias).expect("alias");
        cases.push(Case {
            family: "csharp",
            fixture: "csharp-semantic-spike",
            name: "App.Program.Main",
            language: ResourceLanguageWire::CSharp,
            within: None,
            edit: (
                "src/App/Program.cs",
                "var label = runner.Label();",
                "\n        var again = runner.Compute(2);",
            ),
            locator: format!("install_root = \"{}\"", alias.display()),
            alias,
        });
    }
    #[cfg_attr(not(unix), allow(unused_variables))]
    if let Some(executable) = rust_analyzer() {
        let alias = home.dir.path().join("rust-analyzer-p039");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&executable, &alias).expect("alias");
        cases.push(Case {
            family: "rust",
            fixture: "rust-semantic-spike",
            // `main` alone is ambiguous with build.rs.
            name: "main",
            language: ResourceLanguageWire::Rust,
            within: Some("crates/app/src/main.rs"),
            edit: (
                "crates/app/src/main.rs",
                "let _idle = Idle.run();",
                "\n    let _again = worker.execute();",
            ),
            locator: format!("executable = \"{}\"", alias.display()),
            alias,
        });
    }
    home.config(
        &cases
            .iter()
            .map(|case| format!("\n[semantic_backends.{}]\n{}\n", case.family, case.locator))
            .collect::<String>(),
    );

    for case in &cases {
        let alias = case.alias.to_string_lossy().into_owned();
        // Untrusted (absent) and explicitly Untrusted: the existing
        // degradation -- located, never registered, structural gap stays.
        let mut structural = None;
        for (label, body) in [
            ("absent", String::new()),
            (
                "untrusted",
                "project_execution_trust = \"Untrusted\"\n".to_owned(),
            ),
        ] {
            let root = home.workspace(&format!("{}-{label}", case.family), case.fixture);
            workspace_config(&root, &body);
            let daemon = Daemon::start(&home.global).await;
            let workspace = daemon.init(&root).await;
            let answer =
                outgoing_within(&daemon, workspace, case.name, case.language, case.within).await;
            assert!(semantic_gaps(&answer) >= 1, "{}: {answer:?}", case.family);
            let stats = daemon.semantic(workspace).await;
            assert!(!stats.registered.contains(&case.family), "{stats:?}");
            assert!(
                stats.unavailable[case.family].contains("UNTRUSTED"),
                "{stats:?}"
            );
            assert_eq!(stats.backend_starts, 0);
            assert_eq!(processes_matching(&alias), 0);
            assert!(owner_states(&root).is_empty(), "nothing published");
            structural = Some(answer);
            daemon.stop().await;
        }
        let structural = structural.expect("structural answer");

        // Explicit Workspace Trusted: registered, asleep until demand.
        let root = home.workspace(&format!("{}-trusted", case.family), case.fixture);
        workspace_config(&root, "project_execution_trust = \"Trusted\"\n");
        if case.family == "csharp" {
            // The user's own environment step (#19): a restored solution.
            let restored = Command::new("dotnet")
                .args(["restore", "CSharpSemanticSpike.sln"])
                .current_dir(&root)
                .output()
                .expect("dotnet restore");
            assert!(restored.status.success(), "{restored:?}");
        }
        let daemon = Daemon::start(&home.global).await;
        let workspace = daemon.init(&root).await;
        let stats = daemon.semantic(workspace).await;
        assert!(stats.registered.contains(&case.family), "{stats:?}");
        assert_eq!(stats.backend_starts, 0, "fresh init starts nothing");
        daemon.query(workspace, find_files()).await;
        daemon
            .query(
                workspace,
                QueryOperationWire::Find(FindQueryWire::Target {
                    target: target_for(&daemon, workspace, case.name, case.language, case.within)
                        .await,
                    delivery: delivery(),
                }),
            )
            .await;
        let stats = daemon.semantic(workspace).await;
        assert_eq!(stats.backend_starts, 0, "structural-only: {stats:?}");
        assert_eq!(processes_matching(&alias), 0);

        let started = Instant::now();
        let enriched =
            outgoing_within(&daemon, workspace, case.name, case.language, case.within).await;
        let cold = started.elapsed();
        let stats = daemon.semantic(workspace).await;
        println!(
            "{}: cold {cold:?}; structural calls {} gaps {} requires_semantics {}; \
             enriched calls {} gaps {} requires_semantics {}; {stats:?}",
            case.family,
            calls(&structural),
            structural.gaps.len(),
            semantic_gaps(&structural),
            calls(&enriched),
            enriched.gaps.len(),
            semantic_gaps(&enriched),
        );
        assert_eq!(stats.backend_starts, 1, "{stats:?}");
        assert_eq!(stats.backend_start_successes, 1);
        assert_eq!(stats.live_runtimes, 1);
        assert!(stats.owners_refreshed >= 1, "{stats:?}");
        assert_eq!(stats.owner_failures, 0, "{stats:?}");
        assert_eq!(processes_matching(&alias), 1);
        assert_eq!(enriched.coverage.semantic.contexts, 1, "{enriched:?}");
        assert!(!enriched.coverage.semantic.not_current);
        assert!(
            semantic_gaps(&enriched) < semantic_gaps(&structural),
            "semantics closed gaps: {} -> {}",
            semantic_gaps(&structural),
            semantic_gaps(&enriched)
        );
        assert!(
            calls(&enriched) > calls(&structural),
            "Level A binds calls: {} -> {}",
            calls(&structural),
            calls(&enriched)
        );
        assert!(owner_states(&root).contains(&SemanticState::Current));

        // rust-analyzer's own `cargo metadata` at load raises a watcher
        // event on Cargo.lock (bytes unchanged); the next barrier's
        // existing withdraw-before-refresh then re-proves the owner once,
        // on the same process. Settle that, then measure warm reuse.
        let settled =
            outgoing_within(&daemon, workspace, case.name, case.language, case.within).await;
        assert_eq!(calls(&settled), calls(&enriched));
        let stats = daemon.semantic(workspace).await;
        assert_eq!(stats.backend_starts, 1, "never a respawn: {stats:?}");

        // Warm: same process, nothing re-requested for a current answer.
        let requests = stats.backend_requests;
        let again =
            outgoing_within(&daemon, workspace, case.name, case.language, case.within).await;
        assert_eq!(calls(&again), calls(&enriched));
        let stats = daemon.semantic(workspace).await;
        assert_eq!(stats.backend_starts, 1);
        assert_eq!(stats.backend_requests, requests, "current: no request");
        assert_eq!(processes_matching(&alias), 1);

        // A source edit: structural truth moves on, the prior semantic
        // basis is not current, and the next semantic query delivers the
        // edit to the warm process and re-proves the owner.
        let (file, anchor, inserted) = case.edit;
        let path = root.join(file);
        let source = fs::read_to_string(&path).expect("source");
        assert!(source.contains(anchor), "{file}");
        fs::write(
            &path,
            source.replacen(anchor, &format!("{anchor}{inserted}"), 1),
        )
        .expect("edit");
        let expected = call_sites(&enriched) + 1;
        let mut edited = None;
        for _ in 0..40 {
            let answer =
                outgoing_within(&daemon, workspace, case.name, case.language, case.within).await;
            if call_sites(&answer) == expected {
                edited = Some(answer);
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let edited = edited.unwrap_or_else(|| panic!("{}: the edit's call binds", case.family));
        assert_eq!(semantic_gaps(&edited), 0, "{edited:?}");
        assert!(!edited.coverage.semantic.not_current);
        let stats = daemon.semantic(workspace).await;
        assert_eq!(stats.backend_starts, 1, "no respawn for an edit: {stats:?}");
        assert!(stats.change_notifications >= 1, "{stats:?}");
        assert_eq!(stats.owner_failures, 0, "{stats:?}");
        assert_eq!(processes_matching(&alias), 1);
        daemon.stop().await;
        eventually("the backend shuts down with the daemon", || async {
            processes_matching(&alias) == 0
        })
        .await;
    }
}

/// 17: TypeScript/JavaScript and Svelte through the same product path.
#[tokio::test]
async fn typescript_and_svelte_enrich_through_the_product_path() {
    if installed("typescript_semantic_spike", "typescript") {
        let home = Home::new("typescript");
        home.config(&format!(
            "\n[semantic_backends.typescript]\ninstall_root = \"{}\"\n",
            spike("typescript_semantic_spike").display()
        ));
        let root = home.workspace("ws", "typescript-semantic-spike");
        let daemon = Daemon::start(&home.global).await;
        let workspace = daemon.init(&root).await;
        assert_eq!(daemon.semantic(workspace).await.backend_starts, 0);
        let result = daemon
            .relations(
                workspace,
                symbol("consume", ResourceLanguageWire::TypeScript),
                RelationDirectionWire::Outgoing,
            )
            .await;
        let answer = &result.answers[0];
        assert_eq!(calls(answer), 3, "s.run() and m.describe() bind too");
        assert_eq!(receiver_gaps(answer), 0, "{:?}", answer.gaps);
        assert_eq!(answer.coverage.semantic.contexts, 1);
        let stats = daemon.semantic(workspace).await;
        assert_eq!(stats.registered, vec!["typescript"]);
        assert_eq!(stats.backend_starts, 1);
        assert_eq!(stats.live_runtimes, 1);
        daemon.stop().await;
    }

    if installed("svelte_semantic_spike", "svelte-language-server") {
        let home = Home::new("svelte");
        home.config(&format!(
            "\n[semantic_backends.svelte]\ninstall_root = \"{}\"\n",
            spike("svelte_semantic_spike").display()
        ));
        let root = home.workspace("ws", "svelte-semantic-spike");
        let daemon = Daemon::start(&home.global).await;
        let workspace = daemon.init(&root).await;
        assert_eq!(daemon.semantic(workspace).await.backend_starts, 0);
        let result = daemon
            .relations(
                workspace,
                ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                    "src/Parent.svelte".to_owned(),
                )),
                RelationDirectionWire::Outgoing,
            )
            .await;
        let answer = &result.answers[0];
        // A container-only component: structurally no outgoing evidence,
        // PARTIAL coverage; Level A reaches the template references.
        assert!(
            answer
                .confirmed
                .iter()
                .any(|relation| relation.kind == RelationKindWire::References),
            "{:?}",
            answer.confirmed
        );
        assert_eq!(answer.coverage.semantic.contexts, 1);
        let stats = daemon.semantic(workspace).await;
        assert_eq!(stats.registered, vec!["svelte"]);
        assert_eq!(stats.backend_starts, 1);
        daemon.stop().await;
    }
}
