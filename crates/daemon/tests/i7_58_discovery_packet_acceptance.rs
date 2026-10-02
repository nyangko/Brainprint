//! #58 (first improvement): file/module inspect packet, small-budget
//! partial delivery, ambiguous candidate descriptors -- over the real
//! local IPC, with the MCP default (`compact`) budget.

use std::{
    env, fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use brainprint_core::{
    PROTOCOL_VERSION, WorkspaceId,
    protocol::{
        self, ClientConnection, HandshakeRequest, InitRequest, InitResponse, Request, Response,
        query::*,
    },
};
use brainprint_daemon::{query::DaemonQueryRuntime, runtime_paths, server::Server};
use brainprint_engine::paths::GlobalPaths;

// ------------------------------------------------------------- harness

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "bp-58-{label}-{}-{}",
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

const SHARED_TS: &str = "export function helper(): number {\n  return 42;\n}\n";
const APP_TS: &str = "import { helper } from \"./shared\";\n\nexport function run(): number {\n  return helper();\n}\n";

fn fixture_workspace(label: &str) -> TestDir {
    let workspace = TestDir::create(label);
    let src = workspace.path().join("src");
    fs::create_dir_all(&src).expect("src");
    fs::write(src.join("shared.ts"), SHARED_TS).expect("shared.ts");
    fs::write(src.join("app.ts"), APP_TS).expect("app.ts");
    workspace
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

    async fn send(&self, request: Request) -> Response {
        #[cfg(unix)]
        let connection = ClientConnection::connect(&self.endpoint.socket_path).await;
        #[cfg(windows)]
        let connection = ClientConnection::connect(&self.endpoint.pipe_name).await;
        let mut connection = connection.expect("connect");
        roundtrip(
            &mut connection,
            Request::Handshake(HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_kind: "issue-58".to_owned(),
            }),
        )
        .await;
        roundtrip(&mut connection, request).await
    }

    async fn init(&self, root: &Path) -> WorkspaceId {
        match self
            .send(Request::Init(InitRequest { path: path(root) }))
            .await
        {
            Response::Init(InitResponse { workspace_id, .. }) => workspace_id.parse().expect("id"),
            other => panic!("init failed: {other:?}"),
        }
    }

    async fn query(
        &self,
        workspace: WorkspaceId,
        operation: QueryOperationWire,
    ) -> QueryResultWire {
        let Response::Query(response) = self
            .send(Request::Query(QueryRequest {
                request_id: "8c1d3f5a-2b4e-4d6f-8a0c-1e2f3a4b5c6d".to_owned(),
                workspace: WorkspaceSelectorWire::Id {
                    workspace_id: workspace,
                },
                correlation: None,
                operation,
            }))
            .await
        else {
            panic!("expected a Query response")
        };
        match response.outcome {
            QueryOutcomeWire::Ok(result) => result,
            QueryOutcomeWire::Err(error) => panic!("query error: {error:?}"),
        }
    }
}

async fn roundtrip(connection: &mut ClientConnection, request: Request) -> Response {
    protocol::framing::write_message(connection, &request)
        .await
        .expect("write");
    protocol::framing::read_message(connection)
        .await
        .expect("read")
}

fn path(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

fn budget(items: usize) -> DeliveryWire {
    DeliveryWire {
        budget: DeliveryBudgetWire {
            max_items: NonZeroUsize::new(items),
            max_bytes: NonZeroUsize::new(16 * 1024),
        },
        continuation: None,
        retention: RetentionWire::Disabled,
    }
}

fn answer(result: QueryResultWire) -> ProjectedAnswerWire {
    match result {
        QueryResultWire::Inspect(answer) => answer,
        QueryResultWire::Find(FindResultWire::Target(answer)) => answer,
        other => panic!("expected a projected answer: {other:?}"),
    }
}

fn full(answer: &ProjectedAnswerWire) -> Vec<&EvidenceWire> {
    answer
        .page
        .evidence
        .iter()
        .filter_map(|item| match item {
            DeliveredItemWire::Full(evidence) => Some(evidence),
            DeliveredItemWire::Reuse(_) => None,
        })
        .collect()
}

const MODULE_TS: &str = "\
import { helper } from './helper'

export interface Shape {
  area(): number
}

export function first(): number {
  return helper()
}

export function second(): number {
  return first() + 1
}
";

#[tokio::test]
async fn inspect_a_file_small_budgets_and_ambiguity_over_ipc() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global).await;
    let workspace = fixture_workspace("packet");
    fs::write(workspace.path().join("src/module.ts"), MODULE_TS).expect("module");
    fs::write(
        workspace.path().join("src/helper.ts"),
        "export function helper(): number {\n  return 1\n}\n",
    )
    .expect("helper");
    fs::write(
        workspace.path().join("src/other.ts"),
        "export function helper(): number {\n  return 2\n}\n",
    )
    .expect("other");
    let id = daemon.init(workspace.path()).await;

    // File inspect: outline + the file's own declarations, compact budget.
    let file = answer(
        daemon
            .query(
                id,
                QueryOperationWire::Inspect(InspectWire {
                    target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                        "src/module.ts".to_owned(),
                    )),
                    delivery: budget(16),
                }),
            )
            .await,
    );
    let evidence = full(&file);
    let outline = evidence
        .iter()
        .find_map(|item| match item {
            EvidenceWire::Outline(outline) => Some(outline),
            _ => None,
        })
        .expect("an outline");
    let names: Vec<_> = outline
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(
        ["Shape", "first", "second"]
            .iter()
            .all(|name| names.contains(name)),
        "{names:?}"
    );
    let members: Vec<_> = evidence
        .iter()
        .filter_map(|item| match item {
            EvidenceWire::CurrentSource(range)
                if range.role == RangeRoleWire::MemberDeclaration =>
            {
                Some(range.source.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(
        members
            .iter()
            .any(|source| source.contains("return first() + 1")),
        "{members:?}"
    );

    // A tight item budget: a useful prefix and a continuation, not an error.
    let tight = answer(
        daemon
            .query(
                id,
                QueryOperationWire::Inspect(InspectWire {
                    target: ProjectionTargetWire::Symbol(SymbolTargetWire {
                        name: SymbolNameWire::Name("second".to_owned()),
                        resource: None,
                        kind: None,
                        language: None,
                    }),
                    delivery: budget(2),
                }),
            )
            .await,
    );
    assert_eq!(full(&tight).len(), 2);
    assert!(tight.more_available);
    assert!(tight.continuation.is_some());

    // Ambiguity: each candidate already names its path and kind.
    let ambiguous = answer(
        daemon
            .query(
                id,
                QueryOperationWire::Inspect(InspectWire {
                    target: ProjectionTargetWire::Symbol(SymbolTargetWire {
                        name: SymbolNameWire::Name("helper".to_owned()),
                        resource: None,
                        kind: None,
                        language: None,
                    }),
                    delivery: budget(16),
                }),
            )
            .await,
    );
    let mut candidates: Vec<_> = full(&ambiguous)
        .into_iter()
        .filter_map(|item| match item {
            EvidenceWire::Symbol(candidate) => {
                Some((candidate.path_rel.clone(), candidate.symbol.kind))
            }
            _ => None,
        })
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        candidates,
        [
            ("src/helper.ts".to_owned(), SymbolKindWire::Function),
            ("src/other.ts".to_owned(), SymbolKindWire::Function),
            ("src/shared.ts".to_owned(), SymbolKindWire::Function)
        ]
    );
    daemon.stop().await;
}
