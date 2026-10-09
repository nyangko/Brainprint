//! #56 (I7 task 1): `doctor` + `rebuild` over the real local IPC and the
//! real CLI.
//!
//! Every Workspace state is produced by the daemon (`Init`, its runtime,
//! its watcher); the test only reads files and the index read-only. No
//! semantic backend is configured in these homes, so every rebuild here
//! is also the "backend missing" case.

use std::{
    collections::BTreeMap,
    env, fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{self, Command, Output},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use brainprint_core::{
    PROTOCOL_VERSION, WorkspaceId,
    protocol::{
        self, ClientConnection, ErrorKind, ErrorResponse, HandshakeRequest, InitRequest,
        InitResponse, Request, Response,
        maintenance::{
            BackendStateWire, CheckWire, DatabaseStateWire, DoctorRequest, DoctorResponse,
            DoctorWorkspaceWire, IndexCheckWire, RebuildRequest, RebuildResponse, RuntimeCheckWire,
            SchemaCheckWire, WatcherCheckWire,
        },
        query::*,
    },
};
use brainprint_daemon::{query::DaemonQueryRuntime, runtime_paths, server::Server};
use brainprint_engine::{
    knowledge::{
        NewPolicy, NewWorkItem, PriorityClass, ProjectKnowledgeStore, ProtectionClass, Provenance,
        SourceKind, WorkItemSourceKind, WorkRuntime,
    },
    paths::{GlobalPaths, WorkspacePaths},
};
use rusqlite::{Connection, OpenFlags};

// ------------------------------------------------------------- harness

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "bp-i7t1-{label}-{}-{}",
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
                client_kind: "i7-task1".to_owned(),
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

    async fn doctor(&self, root: &Path) -> DoctorResponse {
        match self
            .send(Request::Doctor(DoctorRequest { path: path(root) }))
            .await
        {
            Response::Doctor(response) => response,
            other => panic!("doctor failed: {other:?}"),
        }
    }

    async fn rebuild(&self, root: &Path) -> Result<RebuildResponse, ErrorResponse> {
        match self
            .send(Request::Rebuild(RebuildRequest { path: path(root) }))
            .await
        {
            Response::Rebuild(response) => Ok(response),
            Response::Error(error) => Err(error),
            other => panic!("unexpected rebuild response: {other:?}"),
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

    /// `inspect` a Symbol: its delivered current source, if resolved and
    /// current.
    async fn inspect(&self, workspace: WorkspaceId, name: &str) -> Option<String> {
        let result = self
            .query(
                workspace,
                QueryOperationWire::Inspect(InspectWire {
                    target: ProjectionTargetWire::Symbol(SymbolTargetWire {
                        name: SymbolNameWire::Name(name.to_owned()),
                        resource: None,
                        kind: None,
                        language: None,
                    }),
                    delivery: DeliveryWire {
                        budget: DeliveryBudgetWire {
                            max_items: NonZeroUsize::new(64),
                            max_bytes: NonZeroUsize::new(64 * 1024),
                        },
                        continuation: None,
                        retention: RetentionWire::Disabled,
                    },
                }),
            )
            .await;
        let QueryResultWire::Inspect(answer) = result else {
            panic!("expected an Inspect answer")
        };
        if !matches!(answer.target_resolution, TargetResolutionWire::Resolved(_))
            || answer.currentness != CurrentnessWire::Current
        {
            return None;
        }
        answer.page.evidence.iter().find_map(|item| match item {
            DeliveredItemWire::Full(EvidenceWire::CurrentSource(range)) => {
                Some(range.source.clone())
            }
            _ => None,
        })
    }

    /// ACTIVE file paths and whether the listing is current.
    async fn files(&self, workspace: WorkspaceId) -> (Vec<String>, bool) {
        let result = self
            .query(
                workspace,
                QueryOperationWire::Find(FindQueryWire::Files {
                    directory: None,
                    recursive: true,
                    path_prefix: None,
                    role: None,
                    language: None,
                    kind: Some(ResourceKindWire::File),
                    limit: NonZeroUsize::new(200).expect("nz"),
                }),
            )
            .await;
        let QueryResultWire::Find(FindResultWire::Files(listing)) = result else {
            panic!("expected a file listing")
        };
        let current = listing.currentness == CurrentnessWire::Current;
        let paths = listing.entries.into_iter().map(|entry| entry.path_rel);
        (paths.collect(), current)
    }

    /// Direct incoming callers of `name`.
    async fn callers(&self, workspace: WorkspaceId, name: &str) -> usize {
        let result = self
            .query(
                workspace,
                QueryOperationWire::Relations(RelationsWire {
                    target: ProjectionTargetWire::Symbol(SymbolTargetWire {
                        name: SymbolNameWire::Name(name.to_owned()),
                        resource: None,
                        kind: None,
                        language: None,
                    }),
                    direction: RelationDirectionWire::Incoming,
                    kinds: vec![RelationKindWire::Calls],
                    delivery: DeliveryWire {
                        budget: DeliveryBudgetWire {
                            max_items: NonZeroUsize::new(64),
                            max_bytes: NonZeroUsize::new(64 * 1024),
                        },
                        continuation: None,
                        retention: RetentionWire::Disabled,
                    },
                }),
            )
            .await;
        let QueryResultWire::Relations(answer) = result else {
            panic!("expected direct relations")
        };
        answer.totals.iter().map(|totals| totals.confirmed).sum()
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

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Every file under `dir` (recursively) with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(path.clone(), fs::read(&path).expect("read"));
            }
        }
    }
    files
}

/// The durable side of a Workspace: identity and config bytes, and every
/// row of every table in project.db and workspace.db. Rows, not file
/// bytes: closing the last connection checkpoints the WAL into the main
/// file, which moves bytes without changing any content.
fn durable(root: &Path) -> BTreeMap<String, Vec<String>> {
    let paths = WorkspacePaths::from_root(root);
    let mut state = BTreeMap::new();
    for file in [&paths.identity_file, &paths.config_file] {
        let text = fs::read_to_string(file).expect("read");
        state.insert(file.display().to_string(), vec![text]);
    }
    for db in [&paths.project_db, &paths.workspace_db] {
        let connection =
            Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("db");
        let tables: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("tables")
            .query_map([], |row| row.get(0))
            .expect("tables")
            .collect::<Result<_, _>>()
            .expect("tables");
        for table in tables {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{table}\""))
                .expect("select");
            let columns = statement.column_count();
            let mut rows: Vec<String> = statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<Result<Vec<_>, _>>()
                        .map(|values| format!("{values:?}"))
                })
                .expect("rows")
                .collect::<Result<_, _>>()
                .expect("rows");
            rows.sort();
            state.insert(format!("{}:{table}", db.display()), rows);
        }
    }
    state
}

fn index_db(root: &Path) -> Connection {
    Connection::open_with_flags(
        WorkspacePaths::from_root(root).index_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("index.db")
}

fn incarnation(root: &Path) -> Vec<u8> {
    index_db(root)
        .query_row(
            "SELECT index_incarnation_uid FROM db_meta WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .expect("incarnation")
}

fn generations(root: &Path) -> Vec<(i64, String)> {
    let connection = index_db(root);
    let mut statement = connection
        .prepare("SELECT generation_no, state FROM generation ORDER BY id")
        .expect("prepare");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// Durable knowledge and Working State, written through the engine's own
/// stores so project.db and workspace.db are not empty.
fn seed_durable(root: &Path, workspace: WorkspaceId) {
    let paths = WorkspacePaths::from_root(root);
    ProjectKnowledgeStore::open(&paths.project_db)
        .expect("project.db")
        .insert_policy(&NewPolicy {
            scope: brainprint_engine::knowledge::KnowledgeScope::project(),
            policy_key: Some("i7-task1-policy".to_owned()),
            title: "keep helper() pure".to_owned(),
            rule_text: "helper() must stay side-effect-free".to_owned(),
            structured_rule: None,
            protection_class: ProtectionClass::Normal,
            priority_class: PriorityClass::Default,
            provenance: Provenance::new(SourceKind::UserExplicit),
        })
        .expect("policy");
    WorkRuntime::open(workspace, &paths.workspace_db, &paths.index_db)
        .expect("work runtime")
        .create(&NewWorkItem {
            source_kind: WorkItemSourceKind::Issue,
            source_ref: Some("#56".to_owned()),
            title: None,
            goal: "survive a rebuild".to_owned(),
        })
        .expect("work item");
}

fn leftovers(root: &Path) -> Vec<String> {
    fs::read_dir(WorkspacePaths::from_root(root).data_dir)
        .expect("data dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.contains(".rebuild"))
        .collect()
}

fn assert_all_unavailable(backends: &[brainprint_core::protocol::maintenance::BackendCheckWire]) {
    assert!(!backends.is_empty(), "every family is reported");
    for backend in backends {
        assert!(
            matches!(backend.state, BackendStateWire::Unavailable { .. }),
            "{backend:?}"
        );
    }
    assert!(
        backends
            .iter()
            .any(|backend| backend.family == "typescript")
    );
}

// =============================================================== doctor

#[tokio::test]
async fn doctor_reports_a_ready_workspace_and_writes_nothing() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global).await;
    let workspace = fixture_workspace("doctor");
    let id = daemon.init(workspace.path()).await;
    seed_durable(workspace.path(), id);

    let source = snapshot(&workspace.path().join("src"));
    let durable_before = durable(workspace.path());
    let generations_before = generations(workspace.path());

    let response = daemon.doctor(workspace.path()).await;
    assert_eq!(response.protocol_version, PROTOCOL_VERSION);
    assert!(matches!(
        response.global_db.state,
        DatabaseStateWire::Opened {
            schema: SchemaCheckWire::Current { .. },
            integrity: CheckWire::Ok
        }
    ));
    let DoctorWorkspaceWire::Initialized(report) = response.workspace else {
        panic!(
            "expected an initialized Workspace: {:?}",
            response.workspace
        )
    };
    assert_eq!(report.workspace_id, id.to_string());
    assert!(report.is_project_home);
    assert_eq!(report.binding, CheckWire::Ok);
    let kinds: Vec<_> = report.databases.iter().map(|db| db.kind.as_str()).collect();
    assert_eq!(kinds, ["project", "workspace", "index"]);
    for db in &report.databases {
        assert!(
            matches!(
                db.state,
                DatabaseStateWire::Opened {
                    schema: SchemaCheckWire::Current { .. },
                    integrity: CheckWire::Ok
                }
            ),
            "{db:?}"
        );
    }
    assert_eq!(report.index, IndexCheckWire::Current);
    assert_eq!(
        report.runtime,
        RuntimeCheckWire::Active {
            watcher: WatcherCheckWire::Attached
        }
    );
    assert_all_unavailable(&report.semantic);

    // Non-destructive: no source, durable or index publication change.
    assert_eq!(snapshot(&workspace.path().join("src")), source);
    assert_eq!(durable(workspace.path()), durable_before);
    assert_eq!(generations(workspace.path()), generations_before);
    daemon.stop().await;
}

#[tokio::test]
async fn doctor_never_creates_or_activates_anything() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global).await;
    let workspace = fixture_workspace("restart");
    let id = daemon.init(workspace.path()).await;
    daemon.stop().await;

    // A fresh daemon process holds no runtime: doctor reports that as it
    // is and does not start one.
    let daemon = Daemon::start(&global).await;
    let response = daemon.doctor(workspace.path()).await;
    let DoctorWorkspaceWire::Initialized(report) = response.workspace else {
        panic!("expected an initialized Workspace")
    };
    assert_eq!(report.runtime, RuntimeCheckWire::Inactive);
    assert!(matches!(report.index, IndexCheckWire::NotMeasured { .. }));
    assert_eq!(daemon.runtime.workspace_runtime_count(), 0);
    assert!(daemon.runtime.lifecycle_stats(id).await.is_none());

    // An uninitialized folder stays uninitialized.
    let plain = fixture_workspace("plain");
    let response = daemon.doctor(plain.path()).await;
    assert!(
        matches!(
            response.workspace,
            DoctorWorkspaceWire::NotInitialized { .. }
        ),
        "{:?}",
        response.workspace
    );
    assert!(!plain.path().join(".brainprint").exists());
    daemon.stop().await;
}

// ============================================================== rebuild

#[tokio::test]
async fn rebuild_keeps_identity_knowledge_and_source_and_serves_at_once() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global).await;
    let workspace = fixture_workspace("rebuild");
    let root = workspace.path();
    let id = daemon.init(root).await;
    seed_durable(root, id);
    assert!(daemon.inspect(id, "run").await.is_some());

    let source = snapshot(&root.join("src"));
    let durable_before = durable(root);
    let incarnation_before = incarnation(root);

    let rebuilt = daemon.rebuild(root).await.expect("rebuild");
    assert_eq!(rebuilt.workspace_id, id.to_string());
    assert!(rebuilt.resources >= 2, "{rebuilt:?}");
    assert_eq!(rebuilt.index, IndexCheckWire::Current);
    assert_eq!(rebuilt.watcher, WatcherCheckWire::Attached);
    // Backend missing: the structural rebuild completes, and no semantic
    // family is claimed.
    assert_all_unavailable(&rebuilt.semantic);

    // Identity, durable knowledge, Working State and source: untouched.
    let durable_after = durable(root);
    assert!(
        durable_before
            .iter()
            .any(|(table, rows)| table.ends_with(":policy") && !rows.is_empty()),
        "seeded knowledge is part of the comparison: {:?}",
        durable_before.keys()
    );
    assert_eq!(durable_after, durable_before);
    assert_eq!(snapshot(&root.join("src")), source);
    // The index is a new incarnation with its own baseline.
    assert_ne!(incarnation(root), incarnation_before);
    assert_eq!(generations(root)[0], (1, "STABLE".to_owned()));
    assert!(leftovers(root).is_empty(), "{:?}", leftovers(root));

    // Served at once: no sync, sleep or restart.
    let (files, current) = daemon.files(id).await;
    assert!(current);
    assert_eq!(files, ["src/app.ts", "src/shared.ts"]);
    assert!(
        daemon
            .inspect(id, "run")
            .await
            .expect("run resolves")
            .contains("helper()")
    );
    assert_eq!(daemon.callers(id, "helper").await, 1);

    // The watcher keeps the rebuilt index current.
    fs::write(
        root.join("src/shared.ts"),
        "export function helper(): number {\n  return 7;\n}\n",
    )
    .expect("edit");
    eventually("the edit reaches the rebuilt index", || async {
        daemon
            .inspect(id, "helper")
            .await
            .is_some_and(|source| source.contains("return 7"))
    })
    .await;

    let after = daemon.doctor(root).await;
    let DoctorWorkspaceWire::Initialized(report) = after.workspace else {
        panic!("expected an initialized Workspace")
    };
    assert_eq!(report.binding, CheckWire::Ok);
    daemon.stop().await;
}

#[tokio::test]
async fn a_failed_rebuild_keeps_the_previous_index() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global).await;
    let workspace = fixture_workspace("failed");
    let root = workspace.path();
    let id = daemon.init(root).await;

    // Something that is not a database sits where the staging index goes.
    let obstacle = PathBuf::from(format!(
        "{}.rebuild",
        WorkspacePaths::from_root(root).index_db.display()
    ));
    fs::create_dir_all(obstacle.join("keep")).expect("obstacle");
    let incarnation_before = incarnation(root);
    let generations_before = generations(root);

    let error = daemon.rebuild(root).await.expect_err("rebuild must fail");
    assert_eq!(error.kind, ErrorKind::DaemonInternal);
    assert!(
        error.message.contains("the previous index is unchanged"),
        "{}",
        error.message
    );
    assert_eq!(incarnation(root), incarnation_before);
    assert_eq!(generations(root), generations_before);
    assert!(daemon.inspect(id, "run").await.is_some());

    fs::remove_dir_all(&obstacle).expect("clear");
    daemon.rebuild(root).await.expect("rebuild after clearing");
    assert_ne!(incarnation(root), incarnation_before);
    daemon.stop().await;
}

#[tokio::test]
async fn rebuild_refuses_an_uninitialized_folder() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global).await;
    let plain = fixture_workspace("plain");
    let error = daemon
        .rebuild(plain.path())
        .await
        .expect_err("nothing to rebuild");
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(!plain.path().join(".brainprint").exists());
    daemon.stop().await;
}

// ================================================================== CLI

fn cli(home: &Path, args: &[&str]) -> Output {
    let mut binary = PathBuf::from(env!("CARGO_BIN_EXE_brainprintd"));
    binary.set_file_name(if cfg!(windows) {
        "brainprint.exe"
    } else {
        "brainprint"
    });
    assert!(binary.is_file(), "build the workspace first");
    Command::new(binary)
        .env(brainprint_core::lifecycle::NO_AUTOSTART_ENV, "1")
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .expect("brainprint")
}

#[tokio::test]
async fn the_cli_runs_doctor_and_rebuild() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global).await;
    let workspace = fixture_workspace("cli");
    let id = daemon.init(workspace.path()).await;

    let run = |args: Vec<String>| {
        let home = home.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            cli(&home, &args)
        })
    };
    let ws = path(workspace.path());

    let doctor = run(vec!["doctor".into(), ws.clone()]).await.expect("cli");
    let text = String::from_utf8_lossy(&doctor.stdout).into_owned();
    assert_eq!(doctor.status.code(), Some(0), "{text}");
    for expected in [
        ["binding:", "ok"],
        ["index:", "current"],
        ["watcher:", "attached"],
    ] {
        assert!(
            text.lines()
                .any(|line| line.split_whitespace().eq(expected.iter().copied())),
            "{expected:?} in\n{text}"
        );
    }
    assert!(text.lines().count() < 30, "compact:\n{text}");

    let json = run(vec!["doctor".into(), ws.clone(), "--json".into()])
        .await
        .expect("cli");
    let parsed: DoctorResponse = serde_json::from_slice(&json.stdout).expect("doctor json");
    assert!(matches!(
        parsed.workspace,
        DoctorWorkspaceWire::Initialized(_)
    ));

    let rebuild = run(vec!["rebuild".into(), ws.clone()]).await.expect("cli");
    let text = String::from_utf8_lossy(&rebuild.stdout).into_owned();
    assert_eq!(rebuild.status.code(), Some(0), "{text}");
    assert!(text.contains(&id.to_string()), "{text}");
    assert!(
        text.lines()
            .any(|line| line.split_whitespace().eq(["index:", "current"])),
        "{text}"
    );

    let plain = fixture_workspace("cli-plain");
    let doctor = run(vec!["doctor".into(), path(plain.path())])
        .await
        .expect("cli");
    assert_eq!(doctor.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("not initialized"));
    daemon.stop().await;
}
